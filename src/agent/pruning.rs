//! 上下文溢出的处置。
//!
//! 两个层次，代价递增：`spill_tool_output` 把大块工具输出转存到磁盘只留引用
//! （落盘时发生，零缓存代价），`handle_overflow` 做真正的压缩（让模型总结掉
//! 旧回合，一次前缀缓存 reset）。
//!
//! 09-07 退役了夹在中间的「机械折叠」层（0.5 提示档 + 0.6 折叠档 +
//! 冷恢复剪枝）：它折的是 `turns.tool_reports`，而那一列从 07-01 起就只装
//! `extract_persistable_tool_report` 的白名单精选小结，从来不装工具输出本体
//! ——实测 410 轮共 ~1.2KB，连单批 12,500 字节的收割闸门都够不着，一次都没
//! 触发过。真正的工具体量在 `tool_flow`，已由 `prune_tool_flow` 在落盘时截断。

use crate::agent::*;

impl Agent {
    /// 用量历史的来源标签:平台回合记平台 id(如 "qq"),其余一律 "agent"。
    /// dsh 式工具输出外溢(spill):模型侧内联超过 context.tool_output_spill_bytes
    /// 的纯文本结果全文存进会话级文件,内联替换为头尾预览+定位提示。read_file
    /// 不外溢(避免 读→溢→再读 循环);存盘失败保留原文,绝不把成功调用变错误。
    /// 只约束进模型的消息,程序侧(报告提取/load_tools 解析等)继续用完整值。
    pub(in crate::agent) fn spill_tool_output(
        &self,
        turn_id: &str,
        call_id: &str,
        tool_name: &str,
        output: &str,
    ) -> Option<String> {
        let cap = self.config.context.tool_output_spill_bytes;
        if cap == 0 || tool_name == "read" || tool_name == "read_file" || output.len() <= cap {
            return None;
        }
        use crate::agent::compact_extras::safe_path_segment as safe_segment;
        let dir = self
            .paths
            .state_dir
            .join("spill")
            .join(safe_segment(&self.state.session_id()));
        if std::fs::create_dir_all(&dir).is_err() {
            return None;
        }
        let file = dir.join(format!(
            "{}-{}-{}.txt",
            safe_segment(turn_id),
            safe_segment(call_id),
            safe_segment(tool_name)
        ));
        let replacement = spill_replacement(output, cap, &file.display().to_string())?;
        if let Err(error) = std::fs::write(&file, output) {
            tracing::warn!(%error, path = %file.display(), "tool output spill failed; keeping inline");
            return None;
        }
        tracing::info!(
            tool = tool_name,
            bytes = output.len(),
            path = %file.display(),
            "oversized tool output spilled"
        );
        Some(replacement)
    }

    /// Derives the verbatim tail budget for compaction. Fixed token count by
    /// design (the trigger scales with the window, the tail does not — that
    /// geometry is what stops the re-compaction loop); chat sessions default
    /// smaller because casual history has less verbatim value.
    pub(in crate::agent) fn compact_tail_budget(&self, context_window: usize) -> usize {
        self.config
            .context
            .compact_tail_tokens
            .unwrap_or(16384.min(context_window / 4))
    }

    /// 压后重建的产出策略。窗口相关的预算缩放交给 `Compactor::with_extras`。
    pub(in crate::agent) fn compact_extras_policy(&self) -> compact_extras::CompactExtrasPolicy {
        let context = &self.config.context;
        // 没有 read 工具时提示语不提「read 回来」:开空头支票只会换来幻觉。
        let read_tool_available = self.tools_enabled && {
            let tools = self.tools.lock().unwrap();
            tools
                .tool_names()
                .iter()
                .any(|name| name == "read" || name == "read_file")
        };
        compact_extras::CompactExtrasPolicy {
            restore_files: context.compact_restore_files,
            restore_file_tokens: context.compact_restore_file_tokens,
            restore_total_tokens: context.compact_restore_total_tokens,
            export_transcript: context.compact_transcript_export,
            read_tool_available,
            transcript_dir: self
                .paths
                .state_dir
                .join("compact")
                .join(compact_extras::safe_path_segment(&self.state.session_id())),
            // 人格、配置、记忆库不回灌:它们走别的通路进上下文,回灌只是
            // 把同一份内容再交一遍。
            exclude_root: Some(self.paths.root_dir.clone()),
            workdir: crate::tools::workspace::effective_workdir(),
        }
    }

    pub(in crate::agent) async fn handle_overflow<F>(
        &self,
        context_tokens: u64,
        on_event: &mut F,
    ) -> Result<Option<compact::CompactResult>>
    where
        F: FnMut(AgentEvent) -> Result<()>,
    {
        use std::sync::atomic::Ordering;
        let context_window = self.context_window();
        let check = overflow::OverflowCheck::new(context_window, self.trim_at_ratio, None);
        let context_tokens = usize::try_from(context_tokens).unwrap_or(usize::MAX);
        if !check.is_enabled() {
            return Ok(None);
        }
        if !check.check_tokens(context_tokens) {
            // Breathing room below the trigger is what a healthy compaction
            // buys; clear the stuck latch and the run counters here, before
            // any other branch can return, so a compaction that settles the
            // context anywhere under the trigger fully re-arms
            // auto-compaction (a stale count would latch the next one off).
            self.consecutive_compacts.store(0, Ordering::Relaxed);
            self.rapid_compacts.store(0, Ordering::Relaxed);
            self.compact_stuck.store(false, Ordering::Relaxed);
            // Below the trigger there is nothing left to do. The sub-trigger
            // watermarks (a 0.5 "context is getting large" notice and a 0.6
            // mechanical fold of `turns.tool_reports`) were retired on 09-07:
            // the fold could never fire, because `tool_reports` holds only the
            // curated report whitelist (`extract_persistable_tool_report`),
            // never bulk tool output — measured at ~1.2 KB across 410 turns
            // against a 12,500-byte harvest gate. The real tool mass lives in
            // `tool_flow` and is already trimmed at write time by
            // `prune_tool_flow`, which costs no cache reset at all. That left
            // the 0.5 notice announcing a fold that could not happen.
            return Ok(None);
        }
        if self.compact_stuck.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let compact_result = match self.on_overflow.as_str() {
            "compact" => {
                let visible_count = self.state.load_visible_turns()?.len();
                if visible_count == 0 {
                    return Ok(None);
                }
                let window = context_window.unwrap();
                let force_threshold =
                    (window as f32 * self.config.context.compact_force_ratio).max(1.0) as usize;
                let force = context_tokens >= force_threshold;
                on_event(AgentEvent::CompactStart)?;
                let compactor = compact::Compactor::new(
                    self.client.clone(),
                    self.state.clone(),
                    window,
                    check.reserved_tokens,
                    self.compact_tail_budget(window),
                    self.preset_dialogs.len(),
                )
                .with_extras(self.compact_extras_policy());
                let mut on_chunk =
                    |chunk: ChatStreamChunk| on_event(AgentEvent::CompactChunk(chunk));
                let fork_builder = |fold_ids: &[String]| -> Result<compact::CompactForkParts> {
                    Ok((
                        self.compact_fork_prefix(fold_ids)?,
                        self.live_tool_definitions()?,
                    ))
                };
                let fork_builder: Option<compact::CompactForkBuilder<'_>> = self
                    .config
                    .context
                    .compact_cache_reuse
                    .then_some(&fork_builder);
                let result = match compactor
                    .perform_compact(force, true, fork_builder, &mut on_chunk)
                    .await
                {
                    Ok(result) => {
                        on_event(AgentEvent::CompactEnd)?;
                        result
                    }
                    Err(e) => {
                        on_event(AgentEvent::CompactEnd)?;
                        return Err(e);
                    }
                };
                if let Some(result) = result.as_ref() {
                    let restored = if result.restored_files > 0 {
                        format!(
                            "{} {}",
                            crate::i18n::text(", restored files", "，回灌文件"),
                            result.restored_files,
                        )
                    } else {
                        String::new()
                    };
                    on_event(AgentEvent::Notice {
                        text: format!(
                            "{} {} → {} {}{}",
                            crate::i18n::text("Compacted: folded turns", "压缩完成：折叠轮次"),
                            result.folded_turns,
                            crate::i18n::text("kept verbatim", "逐字保留最近轮次"),
                            result.kept_turns,
                            restored,
                        ),
                    })?;
                }
                if result.is_some() {
                    // Post-compaction check: still over the trigger means the
                    // verbatim floor plus system prompt alone exceed it.
                    // Twice in a row would re-fire every turn (cratering the
                    // prefix cache each time), so latch auto-compaction off
                    // and say why, once.
                    let post_tokens =
                        usize::try_from(self.effective_context_tokens()?).unwrap_or(usize::MAX);
                    if check.check_tokens(post_tokens) {
                        let runs = self.consecutive_compacts.fetch_add(1, Ordering::Relaxed) + 1;
                        if runs >= 2 && !self.compact_stuck.swap(true, Ordering::Relaxed) {
                            on_event(AgentEvent::Notice {
                                text: crate::i18n::text(
                                    "Automatic context compaction paused: the context window is too small for compaction to help (the system prompt plus the verbatim tail already exceed the trigger). Raise context window or reduce tool output; compaction resumes once the context drops.",
                                    "自动上下文压缩已暂停：上下文窗口太小，压缩无法奏效（system prompt 加逐字尾巴已超过触发线）。请调大上下文窗口或减小工具输出；上下文回落后自动恢复。",
                                )
                                .to_string(),
                            })?;
                        }
                    } else {
                        self.consecutive_compacts.store(0, Ordering::Relaxed);
                    }
                    // Thrashing check: a healthy compaction buys many turns
                    // of breathing room. Refilling within ~3 turns, three
                    // times in a row, means a single oversized item refills
                    // the window and each compaction only craters the cache.
                    let max_seq = self
                        .state
                        .load_visible_turns()?
                        .last()
                        .map(|turn| turn.seq)
                        .unwrap_or(-1);
                    let previous = self.last_compact_max_seq.swap(max_seq, Ordering::Relaxed);
                    // Each turn advances seq by 1 and the compaction summary
                    // itself takes one, so "within 3 turns" is a delta <= 4.
                    if previous >= 0 && max_seq.saturating_sub(previous) <= 4 {
                        let rapid = self.rapid_compacts.fetch_add(1, Ordering::Relaxed) + 1;
                        if rapid >= 3 && !self.compact_stuck.swap(true, Ordering::Relaxed) {
                            on_event(AgentEvent::Notice {
                                text: crate::i18n::text(
                                    "Automatic context compaction paused: the context refills within a few turns of each compaction. A single message or tool output is likely too large for the window — read in smaller chunks, or /clear to start fresh.",
                                    "自动上下文压缩已暂停：每次压缩后几轮内上下文就再次填满。可能有单条消息或工具输出对窗口而言过大——请分块读取，或使用 /clear 重新开始。",
                                )
                                .to_string(),
                            })?;
                        }
                    } else {
                        self.rapid_compacts.store(0, Ordering::Relaxed);
                    }
                } else {
                    // cut=0(可见历史全部落在保尾预算内)却仍越过触发线:
                    // 没有任何东西可折,再触发也只会空转。与"压后仍超"
                    // 走同一失败闩,否则每轮都白跑一次压缩流程。
                    let post_tokens =
                        usize::try_from(self.effective_context_tokens()?).unwrap_or(usize::MAX);
                    if check.check_tokens(post_tokens) {
                        let runs = self.consecutive_compacts.fetch_add(1, Ordering::Relaxed) + 1;
                        if runs >= 2 && !self.compact_stuck.swap(true, Ordering::Relaxed) {
                            on_event(AgentEvent::Notice {
                                text: crate::i18n::text(
                                    "Automatic context compaction paused: the context window is too small for compaction to help (the system prompt plus the verbatim tail already exceed the trigger). Raise context window or reduce tool output; compaction resumes once the context drops.",
                                    "自动上下文压缩已暂停：上下文窗口太小，压缩无法奏效（system prompt 加逐字尾巴已超过触发线）。请调大上下文窗口或减小工具输出；上下文回落后自动恢复。",
                                )
                                .to_string(),
                            })?;
                        }
                    } else {
                        self.consecutive_compacts.store(0, Ordering::Relaxed);
                    }
                }
                result
            }
            "pop" => {
                on_event(AgentEvent::PopStart)?;
                self.trim_visible_context()?;
                on_event(AgentEvent::PopEnd)?;
                None
            }
            _ => None,
        };
        Ok(compact_result)
    }
}
