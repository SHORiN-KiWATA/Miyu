//! 本机 CLI 中转线(claude-code / antigravity / codex)的共用骨架。
//!
//! 三条线的传输都是「拉起一个 CLI 子进程,喂一段 stdin,按行读结构化事件」,
//! 工具循环都在 CLI 侧闭环,Miyu 的工具都经 `miyu mcp-serve` 桥挂进去。各线
//! 只差三样:命令行怎么拼、stdin 长什么样、事件怎么解析。其余——工具作用域
//! 裁决、逐消息哈希链续传、载荷转写、子进程泵、清空联动——都在这里。
//!
//! 续传驱动([`ResumePlan`])刻意做成两步 API 而不是回调:各线的 run 闭包
//! 要可变借用 on_chunk 跨 await,塞进 FnMut→Future 的签名里表达不了;两步
//! 之后每条线只剩十行样板,也更好读。

pub(in crate::llm::openai_compatible) mod payload;
pub(in crate::llm::openai_compatible) mod process;
pub(in crate::llm::openai_compatible) mod session;

use crate::llm::openai_compatible::*;

/// 模式作用域判定:会话是 dev 还是 normal 由 Agent 构造时经
/// `with_claude_code_dev_mode` 打到客户端上。未知值按 off 兜底。
pub(in crate::llm::openai_compatible) fn scope_allows(scope: &str, dev_mode: bool) -> bool {
    match scope.trim().to_ascii_lowercase().as_str() {
        "all" => true,
        "dev" => dev_mode,
        "normal" => !dev_mode,
        _ => false,
    }
}

/// 本轮的双四档裁决结果。
#[derive(Clone, Copy, Debug)]
pub(in crate::llm::openai_compatible) struct ToolScopes {
    pub(in crate::llm::openai_compatible) native_on: bool,
    pub(in crate::llm::openai_compatible) miyu_on: bool,
}

/// 工具面按双四档作用域装配。subagent 作用域也给:中转不会把工具循环交还
/// 给外层(SubagentRunner 收到的永远是最终文本),子代理的干活能力全靠内层
/// CLI 自己的原生工具 + MCP 桥闭环;真正的纯文本辅助请求(摘要/标题/judge)
/// 仍然无工具。
pub(in crate::llm::openai_compatible) fn tool_scopes(
    request_scope: &str,
    native_scope: &str,
    miyu_scope: &str,
    dev_mode: bool,
) -> ToolScopes {
    let tool_capable = matches!(request_scope, "chat" | "subagent");
    ToolScopes {
        native_on: tool_capable && scope_allows(native_scope, dev_mode),
        miyu_on: tool_capable && scope_allows(miyu_scope, dev_mode),
    }
}

/// 本轮经 MCP 桥暴露的工具面档位。判据与桥完全同源:桥问工具时走
/// `attach_owner_turn_tools` → `apply_platform_turn_scope`,取的就是这个活体
/// 平台上下文的 `host_tools_allowed()`。非平台会话(REPL/WebUI/回合外)没有
/// 登记,按全量底座记——那些路径本来就只有 owner 一档。
pub(in crate::llm::openai_compatible) fn host_tools_face(miyu_session: Option<&str>) -> bool {
    miyu_session
        .and_then(crate::platforms::live_turn_context)
        .map(|context| context.host_tools_allowed())
        .unwrap_or(true)
}

/// 中转侧工具活动里**不**翻成卡片事件的工具。桥问答(`ask_question`)有自己
/// 的事件流(question.requested/answered,由 bridge_question 直发 EventHub),
/// 再发一份 tool.started 只会捣乱:CLI 的工具步开始与桥的 question.requested
/// 并发到达,前者晚到时终端的「准备问题」黏性态在面板关掉之后才被置上,
/// 此后每个工具前都挂着「准备问题」(09-03 用户实录)。
pub(in crate::llm::openai_compatible) fn hidden_remote_tool(name: &str) -> bool {
    name == "ask_question"
}

/// 折叠空白并按字符截断,给思考通道的一行摘要用。
pub(in crate::llm::openai_compatible) fn compact_line(text: &str, limit: usize) -> String {
    let mut collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > limit {
        collapsed = collapsed.chars().take(limit).collect::<String>() + "…";
    }
    collapsed
}

/// 按字符截断但保留换行结构:命令输出走命令输出块渲染,折叠空白会把
/// 多行日志压成一行。
pub(in crate::llm::openai_compatible) fn truncate_block(text: &str, limit: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.chars().count() > limit {
        trimmed.chars().take(limit).collect::<String>() + "…"
    } else {
        trimmed.to_string()
    }
}

/// 中转工具结果的展示整形:命令家族保换行,其余折成一行;`\r` 一律去掉。
pub(in crate::llm::openai_compatible) fn shape_remote_output(name: &str, output: &str) -> String {
    let output = output.replace('\r', "");
    if crate::render::is_command_tool(name) {
        truncate_block(&output, 4000)
    } else {
        compact_line(&output, 4000)
    }
}

/// 桥进程要认得 daemon 的 home/runtime 目录,但必须**如实透传**(daemon 自己
/// 有什么才给什么):runtime 目录推导对「显式设了 MIYU_HOME」与「没设」给出
/// 不同路径(默认 home 显式设也会变成哈希子目录),无条件塞 MIYU_HOME 会让
/// mcp-serve 连不上正常启动的 daemon,静默滑进直连兜底(claude 线第六轮实录)。
pub(in crate::llm::openai_compatible) fn bridge_env_passthrough() -> Vec<(String, String)> {
    ["MIYU_HOME", "XDG_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().to_string()))
        })
        .collect()
}

/// 一轮中转的结果:正文结果 + CLI 侧会话 id(续传映射用)。
pub(in crate::llm::openai_compatible) struct RelayOutcome {
    pub(in crate::llm::openai_compatible) result: ChatResult,
    pub(in crate::llm::openai_compatible) session_id: Option<String>,
}

/// 一轮中转的续传计划:哈希链、命中的 CLI 会话、要发的增量。
pub(in crate::llm::openai_compatible) struct ResumePlan {
    provider_id: String,
    model: String,
    miyu_session: Option<String>,
    host_tools: bool,
    ephemeral: bool,
    conversation: Vec<ChatMessage>,
    chain: Vec<u64>,
    resumable: Option<(String, usize)>,
}

impl ResumePlan {
    /// `prompt_seed` 是进哈希链种子的系统提示词(含环境事实):它一变,整条
    /// 链失配,下一轮自然全量重放。辅助请求(scope≠chat)一次一个 CLI 会话,
    /// 不参与续传匹配,免得污染主对话的会话映射。
    #[allow(clippy::too_many_arguments)]
    pub(in crate::llm::openai_compatible) fn new(
        provider_id: &str,
        model: &str,
        prompt_seed: &str,
        conversation: Vec<ChatMessage>,
        request_scope: &str,
        miyu_session: Option<&str>,
        host_tools: bool,
    ) -> Self {
        let ephemeral = request_scope != "chat";
        let chain = session::prefix_chain(provider_id, model, prompt_seed, &conversation);
        let resumable = if ephemeral {
            None
        } else {
            session::find_resumable(
                provider_id,
                model,
                miyu_session,
                host_tools,
                &chain,
                conversation.len(),
            )
        };
        Self {
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            miyu_session: miyu_session.map(str::to_string),
            host_tools,
            ephemeral,
            conversation,
            chain,
            resumable,
        }
    }

    pub(in crate::llm::openai_compatible) fn ephemeral(&self) -> bool {
        self.ephemeral
    }

    /// 命中的 CLI 会话 id(要 `--resume`/`--conversation`/`resume` 的目标)。
    pub(in crate::llm::openai_compatible) fn resume_id(&self) -> Option<&str> {
        self.resumable.as_ref().map(|(id, _)| id.as_str())
    }

    /// 本轮要发给 CLI 的增量:命中续传就只发未覆盖的尾巴,否则整段。
    pub(in crate::llm::openai_compatible) fn delta(&self) -> &[ChatMessage] {
        let covered = self.resumable.as_ref().map(|(_, len)| *len).unwrap_or(0);
        &self.conversation[covered..]
    }

    pub(in crate::llm::openai_compatible) fn conversation(&self) -> &[ChatMessage] {
        &self.conversation
    }

    /// CLI 侧会话没了(过期/被清理/静默新开):忘掉映射,改成整段全量重放。
    /// 只对「会话找不到」类错误自愈,限流/登录错误照常上抛——调用方判定。
    pub(in crate::llm::openai_compatible) fn resume_lost(
        &mut self,
        kind: &str,
        request_id: &str,
        error: &anyhow::Error,
    ) {
        tracing::warn!(
            request_id,
            error = %format!("{error:#}"),
            "{kind} resume target is gone; replaying the full conversation in a fresh session"
        );
        if let Some((id, _)) = self.resumable.take() {
            session::forget_session(&id);
        }
    }

    /// 回合结束:预测下一轮的前缀(已发送的会话消息 + 一条 assistant 正文)
    /// 并记下 CLI 会话 id。预测若与实际化石有分歧,下一轮匹配不上,自动退化为
    /// 全量重放——只损失效率,不损失正确性。辅助请求不记。
    pub(in crate::llm::openai_compatible) fn record(&self, outcome: &RelayOutcome) {
        if self.ephemeral {
            return;
        }
        let Some(session_id) = &outcome.session_id else {
            return;
        };
        let content = outcome.result.content.clone();
        if content.trim().is_empty() {
            return;
        }
        let predicted = ChatMessage::assistant(content, None);
        let next_hash = session::extend_chain(self.chain[self.conversation.len()], &predicted);
        session::record_session(
            &self.provider_id,
            &self.model,
            self.miyu_session.as_deref(),
            self.host_tools,
            self.conversation.len() + 1,
            next_hash,
            session_id.clone(),
        );
    }
}

/// 清空 Miyu 会话时的联动(三条 CLI 中转线共用):丢弃它名下的续传映射,
/// 并尽力删除各家 CLI 侧的会话转录。存储布局是各家 CLI 的内部实现,删不到
/// 只记日志不报错——映射已丢弃,该会话无论如何不会再被续传。会话 id 都是
/// 全局唯一,每家都试一遍不会误删。
pub(crate) fn forget_relay_sessions(miyu_session: &str) {
    let removed = session::forget_miyu_session(miyu_session);
    if removed.is_empty() {
        return;
    }
    for relay_session in &removed {
        super::claude_code::remove_transcript(miyu_session, relay_session);
        super::antigravity::remove_conversation_files(relay_session);
        super::codex::remove_rollout(relay_session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_follow_request_scope_and_mode() {
        let scopes = tool_scopes("chat", "all", "dev", false);
        assert!(scopes.native_on && !scopes.miyu_on);
        let scopes = tool_scopes("subagent", "normal", "all", true);
        assert!(!scopes.native_on && scopes.miyu_on);
        let scopes = tool_scopes("compact", "all", "all", false);
        assert!(!scopes.native_on && !scopes.miyu_on);
    }

    #[test]
    fn remote_output_shaping_keeps_command_newlines() {
        assert_eq!(shape_remote_output("run_command", "a\r\nb\r\n"), "a\nb");
        assert_eq!(shape_remote_output("Bash", "a\nb\n"), "a\nb");
        assert_eq!(shape_remote_output("use_meme", "a\nb"), "a b");
        let long = "x".repeat(4001);
        assert!(truncate_block(&long, 4000).ends_with('…'));
    }

    #[test]
    fn plan_prefers_full_replay_for_auxiliary_scopes() {
        let conversation = vec![ChatMessage::plain("user", "hi")];
        let plan = ResumePlan::new("p", "m", "seed", conversation, "compact", None, true);
        assert!(plan.ephemeral());
        assert!(plan.resume_id().is_none());
        assert_eq!(plan.delta().len(), 1);
    }
}
