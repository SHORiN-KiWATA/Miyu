//! 检索与摘要片段。
//!
//! 中文没有空格，所以 `query_tokens` 要自己切（`flush_chinese`）。片段选取
//! （`best_window`）按命中词的密集程度挑窗口，而不是简单取第一处——第一处常常
//! 是目录或页眉。
//!
//! 语义与关键词的结果靠 `merge_results` 融合，文件名匹配单独加权
//! （`score_file_name`）：找「配置」时叫 config.md 的文件就该排前面。

use crate::tools::knowledge_base::*;

pub(in crate::tools::knowledge_base) struct SearchResult {
    pub(in crate::tools::knowledge_base) path: String,
    pub(in crate::tools::knowledge_base) score: f32,
    pub(in crate::tools::knowledge_base) snippets: Vec<String>,
    pub(in crate::tools::knowledge_base) source: &'static str,
}

impl SearchResult {
    pub(in crate::tools::knowledge_base) fn new(
        path: String,
        score: f32,
        snippets: Vec<String>,
        source: &'static str,
    ) -> Self {
        Self {
            path,
            score,
            snippets,
            source,
        }
    }

    pub(in crate::tools::knowledge_base) fn to_json(&self) -> Value {
        json!({
            "path": self.path,
            "name": file_name(&self.path),
            "directory": directory_name(&self.path),
            "score": (self.score * 10.0).round() / 10.0,
            "source": self.source,
            "snippets": self.snippets,
        })
    }
}

pub(in crate::tools::knowledge_base) struct Chunk {
    pub(in crate::tools::knowledge_base) index: usize,
    pub(in crate::tools::knowledge_base) start: usize,
    pub(in crate::tools::knowledge_base) end: usize,
    pub(in crate::tools::knowledge_base) text: String,
}

pub(in crate::tools::knowledge_base) fn query_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut chinese = Vec::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            ascii.push(ch.to_ascii_lowercase());
            flush_chinese(&mut chinese, &mut tokens);
        } else if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            if !ascii.is_empty() {
                tokens.push(std::mem::take(&mut ascii));
            }
            chinese.push(ch);
        } else {
            if !ascii.is_empty() {
                tokens.push(std::mem::take(&mut ascii));
            }
            flush_chinese(&mut chinese, &mut tokens);
        }
    }
    if !ascii.is_empty() {
        tokens.push(ascii);
    }
    flush_chinese(&mut chinese, &mut tokens);
    let mut seen = HashSet::new();
    tokens
        .into_iter()
        .filter(|token| token.chars().count() > 1 || !token.is_ascii())
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

pub(in crate::tools::knowledge_base) fn flush_chinese(
    chars: &mut Vec<char>,
    tokens: &mut Vec<String>,
) {
    if chars.is_empty() {
        return;
    }
    let text = chars.iter().collect::<String>();
    tokens.push(text);
    for window in chars.windows(2) {
        tokens.push(window.iter().collect());
    }
    chars.clear();
}

pub(in crate::tools::knowledge_base) fn find_positions(
    content: &str,
    needle: &str,
    limit: usize,
) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut start = 0;
    while let Some(pos) = content[start..].find(needle) {
        let absolute = start + pos;
        positions.push(absolute);
        if positions.len() >= limit {
            break;
        }
        start = absolute + needle.len().max(1);
    }
    positions
}

pub(in crate::tools::knowledge_base) fn best_window(
    positions_by_token: &HashMap<String, Vec<usize>>,
    tokens: &[String],
    window_chars: usize,
) -> Option<(usize, usize, f32)> {
    let mut events = Vec::new();
    for token in tokens {
        for pos in positions_by_token.get(token).into_iter().flatten() {
            events.push((*pos, token.as_str()));
        }
    }
    events.sort_by_key(|event| event.0);
    let mut best = None;
    for left in 0..events.len() {
        let mut seen = HashSet::new();
        let start = events[left].0;
        let mut end = start;
        for (pos, token) in events.iter().skip(left) {
            if *pos - start > window_chars {
                break;
            }
            seen.insert(*token);
            end = *pos + token.len();
        }
        let coverage = seen.len() as f32 / tokens.len().max(1) as f32;
        if best.map(|(_, _, score)| coverage > score).unwrap_or(true) {
            best = Some((start, end, coverage));
        }
    }
    best.filter(|(_, _, coverage)| *coverage > 0.0)
}

pub(in crate::tools::knowledge_base) fn extract_snippets(
    content: &str,
    content_lower: &str,
    tokens: &[String],
    context: usize,
) -> Vec<String> {
    let mut snippets = Vec::new();
    for token in tokens {
        if let Some(pos) = content_lower.find(token) {
            snippets.push(snippet_chars(content, pos, pos + token.len(), context));
        }
        if snippets.len() >= 3 {
            break;
        }
    }
    if snippets.is_empty() && !content.trim().is_empty() {
        snippets.push(compact_whitespace(
            &content.chars().take(context * 2).collect::<String>(),
        ));
    }
    snippets
}

pub(in crate::tools::knowledge_base) fn snippet_chars(
    content: &str,
    start: usize,
    end: usize,
    context: usize,
) -> String {
    let start = content[..start.min(content.len())]
        .char_indices()
        .rev()
        .nth(context)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let end = content[end.min(content.len())..]
        .char_indices()
        .nth(context)
        .map(|(idx, _)| end.min(content.len()) + idx)
        .unwrap_or(content.len());
    compact_whitespace(&content[start..end])
}

pub(in crate::tools::knowledge_base) fn build_chunks(
    content: &str,
    chunk_chars: usize,
    overlap: usize,
) -> Vec<Chunk> {
    let chars = content.char_indices().collect::<Vec<_>>();
    let mut chunks = Vec::new();
    let mut start_char = 0usize;
    let mut index = 0usize;
    let total_chars = content.chars().count();
    while start_char < total_chars {
        let end_char = (start_char + chunk_chars).min(total_chars);
        let start_byte = chars.get(start_char).map(|(idx, _)| *idx).unwrap_or(0);
        let end_byte = chars
            .get(end_char)
            .map(|(idx, _)| *idx)
            .unwrap_or(content.len());
        let text = content[start_byte..end_byte].to_string();
        if !text.trim().is_empty() {
            chunks.push(Chunk {
                index,
                start: start_byte,
                end: end_byte,
                text,
            });
            index += 1;
        }
        if end_char >= total_chars {
            break;
        }
        start_char = end_char.saturating_sub(overlap).max(start_char + 1);
    }
    chunks
}

pub(in crate::tools::knowledge_base) fn merge_results(
    results: &mut Vec<SearchResult>,
    semantic: Vec<SearchResult>,
    limit: usize,
) {
    for item in semantic {
        if let Some(existing) = results.iter_mut().find(|result| result.path == item.path) {
            existing.score += item.score * 0.6;
            existing.snippets.extend(item.snippets);
            existing.snippets.truncate(4);
        } else {
            results.push(item);
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
}

pub(in crate::tools::knowledge_base) fn score_file_name(
    query: &str,
    name: &str,
) -> (f64, &'static str) {
    let query = query.replace('\\', "/").to_ascii_lowercase();
    let name = name.replace('\\', "/").to_ascii_lowercase();
    let base = file_name(&name);
    if query == name {
        (1000.0, "exact_path")
    } else if query == base {
        (950.0, "exact_file_name")
    } else if name.contains(&query) {
        (820.0 + query.len().min(60) as f64, "path_contains")
    } else if base.contains(&query) {
        (760.0 + query.len().min(60) as f64, "file_name_contains")
    } else {
        let tokens = query_tokens(&query);
        let matched = tokens.iter().filter(|token| name.contains(*token)).count();
        if matched == 0 {
            (0.0, "")
        } else {
            (300.0 + matched as f64 * 80.0, "partial_name_terms")
        }
    }
}

pub(in crate::tools::knowledge_base) fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (a, b) in left.iter().zip(right) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}
