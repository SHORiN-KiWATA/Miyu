use crate::llm::{ChatMessage, ChatResult, ChatStreamChunk, OpenAiCompatibleClient, Usage};
use crate::prompts::COMPACT_SYSTEM_PROMPT;
use crate::state::{StateStore, Turn};
use anyhow::{bail, Result};

use super::overflow::estimate_tokens;

const COMPACT_PROMPT_OVERHEAD: usize = 2000;
const MAX_MERGE_ROUNDS: usize = 5;

pub struct Compactor {
    client: OpenAiCompatibleClient,
    state: StateStore,
    context_window: usize,
    reserved_tokens: usize,
}

pub struct CompactResult {
    pub usage: Usage,
    pub usage_estimated: bool,
}

struct CompactTextResult {
    text: String,
    usage: Usage,
    usage_estimated: bool,
}

impl Compactor {
    pub fn new(
        client: OpenAiCompatibleClient,
        state: StateStore,
        context_window: usize,
        reserved_tokens: usize,
    ) -> Self {
        Self {
            client,
            state,
            context_window,
            reserved_tokens,
        }
    }

    pub async fn perform_compact<F>(&self, on_chunk: &mut F) -> Result<Option<CompactResult>>
    where
        F: FnMut(ChatStreamChunk) -> Result<()>,
    {
        let turns = self.state.load_visible_turns()?;
        if turns.is_empty() {
            return Ok(None);
        }
        if turns
            .iter()
            .any(|turn| turn.status == crate::state::TurnStatus::Running)
        {
            bail!("cannot compact while another conversation turn is running");
        }

        let head: Vec<&Turn> = turns.iter().filter(|turn| !turn.is_summary).collect();
        if head.is_empty() {
            return Ok(None);
        }
        let previous_summary = self.state.load_last_summary()?;
        let prev_text = previous_summary
            .as_ref()
            .map(|t| t.assistant_content.clone());

        let usable = self
            .context_window
            .saturating_sub(self.reserved_tokens)
            .saturating_sub(COMPACT_PROMPT_OVERHEAD);

        let head_text = turns_to_text(&head);
        let head_tokens = estimate_tokens(&head_text);

        let mut compact_usage = Usage::default();
        let mut usage_estimated = false;

        let summary = if head_tokens <= usable {
            let result =
                compact_single_pass(&self.client, &head_text, prev_text.as_deref(), on_chunk)
                    .await?;
            add_usage(&mut compact_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            result.text
        } else {
            let segments = split_into_segments(&head, usable);
            let mut summaries = Vec::new();
            for segment in &segments {
                let segment_text = turns_to_text(segment);
                let result =
                    compact_single_pass(&self.client, &segment_text, None, &mut |_| Ok(())).await?;
                add_usage(&mut compact_usage, &result.usage);
                usage_estimated |= result.usage_estimated;
                summaries.push(result.text);
            }
            let result = merge_summaries_tree(
                &self.client,
                &summaries,
                prev_text.as_deref(),
                usable,
                on_chunk,
            )
            .await?;
            add_usage(&mut compact_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            result.text
        };

        let last_seq = turns.last().unwrap().seq;
        let visible_turn_ids = turns
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        self.state.replace_visible_with_summary(
            last_seq,
            &visible_turn_ids,
            &summary,
            Some(compact_usage.effective_total_tokens()),
            usage_estimated,
        )?;
        Ok(Some(CompactResult {
            usage: compact_usage,
            usage_estimated,
        }))
    }
}

fn add_usage(total: &mut Usage, usage: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    total.total_tokens = total
        .total_tokens
        .saturating_add(usage.effective_total_tokens());
}

fn turns_to_text(turns: &[&Turn]) -> String {
    let mut output = String::new();
    for (i, turn) in turns.iter().enumerate() {
        if turn.is_summary {
            continue;
        }
        output.push_str(&format!("--- Turn {} ---\n", i + 1));
        output.push_str("User: ");
        output.push_str(&turn.user_content);
        for exchange in &turn.question_exchanges {
            output.push_str("\nAssistant clarification: ");
            output.push_str(&crate::question::assistant_exchange_text(exchange));
            output.push_str("\nUser clarification: ");
            output.push_str(&crate::question::user_exchange_text(exchange));
        }
        output.push_str("\nAssistant: ");
        output.push_str(&turn.assistant_content);
        if let Some(reasoning) = &turn.assistant_reasoning {
            if !reasoning.trim().is_empty() {
                output.push_str("\n[Reasoning: ");
                output.push_str(reasoning);
                output.push(']');
            }
        }
        for report in &turn.tool_reports {
            output.push_str("\n[Tool Report: ");
            output.push_str(report);
            output.push(']');
        }
        output.push('\n');
    }
    output
}

fn build_compact_prompt(history: &str, previous_summary: Option<&str>) -> String {
    match previous_summary {
        Some(prev) => format!(
            "Update the anchored summary below using the conversation history above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{prev}\n</previous-summary>\n\n\
             <conversation-history>\n{history}\n</conversation-history>"
        ),
        None => format!(
            "Create a new anchored summary from the conversation history.\n\n\
             <conversation-history>\n{history}\n</conversation-history>"
        ),
    }
}

async fn compact_single_pass<F>(
    client: &OpenAiCompatibleClient,
    history: &str,
    previous_summary: Option<&str>,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let prompt = build_compact_prompt(history, previous_summary);
    let messages = vec![
        ChatMessage::system(COMPACT_SYSTEM_PROMPT.to_string()),
        ChatMessage::plain("user", &prompt),
    ];
    let result = client
        .chat_stream(messages.clone(), vec![], on_chunk)
        .await?;
    Ok(compact_text_result(result, &messages))
}

async fn compact_single_pass_text<F>(
    client: &OpenAiCompatibleClient,
    text: &str,
    previous_summary: Option<&str>,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    let prompt = match previous_summary {
        Some(prev) => format!(
            "Update the anchored summary below using the segment summaries above.\n\
             Preserve still-true details, remove stale details, and merge in the new facts.\n\
             <previous-summary>\n{prev}\n</previous-summary>\n\n\
             <segment-summaries>\n{text}\n</segment-summaries>"
        ),
        None => format!(
            "Merge the following segment summaries into a single coherent summary.\n\n\
             <segment-summaries>\n{text}\n</segment-summaries>"
        ),
    };
    let messages = vec![
        ChatMessage::system(COMPACT_SYSTEM_PROMPT.to_string()),
        ChatMessage::plain("user", &prompt),
    ];
    let result = client
        .chat_stream(messages.clone(), vec![], on_chunk)
        .await?;
    Ok(compact_text_result(result, &messages))
}

fn compact_text_result(result: ChatResult, messages: &[ChatMessage]) -> CompactTextResult {
    if let Some(usage) = result.usage {
        return CompactTextResult {
            text: result.content,
            usage,
            usage_estimated: result.usage_estimated,
        };
    }

    let prompt_tokens = super::overflow::estimate_messages_tokens(messages) as u64;
    let completion_tokens = estimate_tokens(&result.content) as u64;
    CompactTextResult {
        text: result.content,
        usage: Usage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens.saturating_add(completion_tokens),
        },
        usage_estimated: true,
    }
}

fn split_into_segments<'a>(turns: &[&'a Turn], budget_tokens: usize) -> Vec<Vec<&'a Turn>> {
    let mut segments = Vec::new();
    let mut current = Vec::new();
    let mut current_tokens = 0usize;

    for turn in turns {
        let turn_tokens = estimate_tokens(&turn_to_text(turn));
        if current_tokens + turn_tokens > budget_tokens && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current.push(*turn);
        current_tokens += turn_tokens;
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn turn_to_text(turn: &Turn) -> String {
    let mut output = String::new();
    output.push_str(&turn.user_content);
    for exchange in &turn.question_exchanges {
        output.push_str(&crate::question::assistant_exchange_text(exchange));
        output.push_str(&crate::question::user_exchange_text(exchange));
    }
    output.push_str(&turn.assistant_content);
    if let Some(reasoning) = &turn.assistant_reasoning {
        output.push_str(reasoning);
    }
    for report in &turn.tool_reports {
        output.push_str(report);
    }
    output
}

async fn merge_summaries_tree<F>(
    client: &OpenAiCompatibleClient,
    summaries: &[String],
    previous_summary: Option<&str>,
    usable_tokens: usize,
    on_chunk: &mut F,
) -> Result<CompactTextResult>
where
    F: FnMut(ChatStreamChunk) -> Result<()>,
{
    if summaries.len() == 1 {
        return Ok(CompactTextResult {
            text: summaries[0].clone(),
            usage: Usage::default(),
            usage_estimated: false,
        });
    }

    let mut current: Vec<String> = summaries.to_vec();
    let mut total_usage = Usage::default();
    let mut usage_estimated = false;

    for _round in 0..MAX_MERGE_ROUNDS {
        let combined = current.join("\n\n---\n\n");
        let combined_tokens = estimate_tokens(&combined);

        if combined_tokens <= usable_tokens {
            let result =
                compact_single_pass_text(client, &combined, previous_summary, on_chunk).await?;
            add_usage(&mut total_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            return Ok(CompactTextResult {
                text: result.text,
                usage: total_usage,
                usage_estimated,
            });
        }

        let mut next = Vec::new();
        let mut batch = Vec::new();
        let mut batch_tokens = 0usize;

        for s in &current {
            let s_tokens = estimate_tokens(s);
            if batch_tokens + s_tokens > usable_tokens && !batch.is_empty() {
                let batch_text = batch.join("\n\n---\n\n");
                let merged =
                    compact_single_pass_text(client, &batch_text, None, &mut |_| Ok(())).await?;
                add_usage(&mut total_usage, &merged.usage);
                usage_estimated |= merged.usage_estimated;
                next.push(merged.text);
                batch.clear();
                batch_tokens = 0;
            }
            batch.push(s.clone());
            batch_tokens += s_tokens;
        }
        if !batch.is_empty() {
            let batch_text = batch.join("\n\n---\n\n");
            let merged =
                compact_single_pass_text(client, &batch_text, None, &mut |_| Ok(())).await?;
            add_usage(&mut total_usage, &merged.usage);
            usage_estimated |= merged.usage_estimated;
            next.push(merged.text);
        }

        if next.len() >= current.len() {
            let combined = current.join("\n\n---\n\n");
            let result =
                compact_single_pass_text(client, &combined, previous_summary, on_chunk).await?;
            add_usage(&mut total_usage, &result.usage);
            usage_estimated |= result.usage_estimated;
            return Ok(CompactTextResult {
                text: result.text,
                usage: total_usage,
                usage_estimated,
            });
        }
        current = next;
    }

    let combined = current.join("\n\n---\n\n");
    let result = compact_single_pass_text(client, &combined, previous_summary, on_chunk).await?;
    add_usage(&mut total_usage, &result.usage);
    usage_estimated |= result.usage_estimated;
    Ok(CompactTextResult {
        text: result.text,
        usage: total_usage,
        usage_estimated,
    })
}
