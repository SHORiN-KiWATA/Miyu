//! 重做一个已完成的回合。
//!
//! 和正常回合的区别在于**前缀是给定的**：要从检查点恢复到那一刻的消息状态，
//! 再从那里往下跑。中途失败要能退回原样，所以先写备份再动历史。

use crate::agent::*;

impl Agent {
    pub(in crate::agent) async fn redo_stream_turn<F>(
        &mut self,
        candidate: &RedoCandidate,
        prompts: Vec<RedoPromptInput>,
        control: &AgentTurnControl,
        on_event: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        self.cancel_cache_keepalive();
        self.state.recover_stale_turns()?;
        self.trim_visible_context()?;
        if prompts.is_empty()
            || prompts.last().map(|prompt| prompt.prompt_id.as_str())
                != Some(candidate.input_id.as_str())
        {
            bail!("redo prompts no longer match the selected input");
        }
        let current_turn = self
            .state
            .load_turns()?
            .into_iter()
            .find(|turn| turn.turn_id == candidate.turn_id)
            .context("redo turn no longer exists")?;

        let mut prepared = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let input = self
                .prepare_user_input(&prompt.content, &prompt.images)
                .await?;
            prepared.push((prompt, input));
        }
        let (last_prompt, last_input) = prepared.last().context("redo input is empty")?;
        let last_content = last_input.content.clone();
        let last_display_content = last_prompt.display_content.clone();
        let diary_input = last_content.clone();
        let redo = self.state.begin_redo(
            &candidate.turn_id,
            &candidate.input_id,
            candidate.input_kind,
            candidate.revision,
            &last_content,
            &last_display_content,
            std::process::id(),
        )?;
        let guard =
            PendingRedoGuard::new(self.state.clone(), candidate.turn_id.clone(), redo.revision);
        let mut on_event = on_event;
        on_event(AgentEvent::TurnStarted {
            turn_id: candidate.turn_id.clone(),
        })?;

        let (mut messages, redo_user_index) = self.chat_messages(&candidate.turn_id, "")?;
        // 按下标摘下 [占位用户, 瞬态尾巴...]:重放輸入接回后尾巴原样跟上,
        // 保持"瞬态永远在用户消息之后"。
        let tail_fossils = messages.split_off(redo_user_index + 1);
        let _ = messages.pop();
        let replay_start;
        let fossil_start;
        let base_tool_reports;
        let initial_tool_rounds;
        let initial_question_rounds;
        match candidate.input_kind {
            RedoInputKind::Initial => {
                let (_, input) = prepared.pop().context("redo input is empty")?;
                messages.push(input.message);
                fossil_start = messages.len();
                messages.extend(tail_fossils);
                replay_start = messages.len();
                messages.extend(input.hints);
                base_tool_reports = Vec::new();
                initial_tool_rounds = 0;
                initial_question_rounds = 0;
            }
            RedoInputKind::Followup => {
                let checkpoint = redo.checkpoint.context("redo checkpoint is unavailable")?;
                messages.push(self.turn_user_message(&current_turn));
                fossil_start = messages.len();
                messages.extend(tail_fossils);
                replay_start = messages.len();
                messages.extend(checkpoint.replay_messages);
                for (_, input) in prepared {
                    messages.push(input.message);
                    messages.extend(input.hints);
                }
                base_tool_reports = checkpoint.prefix_tool_reports;
                initial_tool_rounds = checkpoint.tool_rounds;
                initial_question_rounds = checkpoint.question_rounds;
            }
        }
        // Redo rewrites the turn, so refresh its fossilized tail to match what
        // this generation actually sends (new runtime stamp + replayed tail).
        self.state.set_turn_context_messages(
            &candidate.turn_id,
            &fossil_context_messages(&messages[fossil_start..]),
        )?;

        let mut used_tools = Vec::new();
        let mut persisted_tool_reports = Vec::new();
        let mut journal =
            TurnJournalSink::new(self.state.clone(), candidate.turn_id.clone(), redo.revision);
        let stream_result = {
            let mut journaled_event = |event| journal.emit(event, &mut on_event);
            self.chat_with_tools(
                &candidate.turn_id,
                &mut messages,
                &mut used_tools,
                &mut persisted_tool_reports,
                replay_start,
                &base_tool_reports,
                initial_tool_rounds,
                initial_question_rounds,
                Some(control),
                &mut journaled_event,
            )
            .await
        };
        journal.finish(&mut on_event)?;
        let result = stream_result?;
        let reports = persisted_tool_reports
            .into_iter()
            .map(|(_, report)| report)
            .collect::<Vec<_>>();
        self.state
            .append_persisted_contexts(&candidate.turn_id, &reports)?;
        let tokens = TurnTokens::from_usage(result.usage.as_ref());
        guard.complete_with_model(
            &result.content,
            result.reasoning.as_deref(),
            result.provider_id.as_deref(),
            result.model.as_deref(),
            tokens,
            result.usage_estimated,
        )?;
        self.state.set_turn_context_end(
            &candidate.turn_id,
            crate::agent::context_meter::context_end_tokens(&result),
        )?;
        if let Some(usage) = result.usage.as_ref() {
            if usage.generation_tokens > 0 && usage.generation_ms > 0 {
                self.state.set_turn_generation(
                    &candidate.turn_id,
                    usage.generation_tokens,
                    usage.generation_ms,
                )?;
            }
        }
        if let (Some(provider), Some(model)) = (&result.provider_id, &result.model) {
            self.last_request_endpoint = Some((provider.clone(), model.clone()));
        }
        // 无条件覆盖:redo 前的旧修订可能留有 tool_flow,新修订没有工具
        // 调用时空 flow 也必须写入,否则旧工具流会被冒名回放。
        let mut tool_flow = derive_tool_flow(&messages, replay_start);
        prune_tool_flow(&mut tool_flow, &self.config.context);
        self.append_remote_tool_flow(&mut tool_flow);
        self.state
            .set_turn_tool_flow(&candidate.turn_id, &tool_flow)?;
        if self.memory.process_after_turn(
            &diary_input,
            &result.content,
            &self.memory_origin,
            &self.memory_database_id,
            self.memory_generation,
        )? {
            self.wake_memory_organizer();
        }
        if let Some(usage) = result.usage.clone() {
            let meta = crate::state::UsageMeta {
                source: self.usage_source(),
                provider: result.provider_id.as_deref(),
                model: result.model.as_deref(),
                kind: None,
            };
            self.state.add_usage(&usage, meta)?;
        }
        self.start_cache_keepalive();
        Ok(result)
    }

    /// Publishes the turn's session as the ambient scope before running it.
    /// Subagents launched inside read it to hang their audit sessions off this
    /// one, and those audits now count toward the session's Σ — without the
    /// scope a subagent bills to nobody. The daemon actor sets the same scope;
    /// re-scoping to the same id is harmless, and the direct/local paths had
    /// no scope at all.
    pub(in crate::agent) async fn chat_stream_with_images_inner<F>(
        &mut self,
        input: &str,
        images: &[Option<PastedImage>],
        control: Option<&AgentTurnControl>,
        on_event: F,
    ) -> Result<ChatResult>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        let session = self.state.session_id();
        crate::tools::workspace::with_session(
            session,
            self.chat_stream_turn(input, images, control, on_event),
        )
        .await
    }
}
