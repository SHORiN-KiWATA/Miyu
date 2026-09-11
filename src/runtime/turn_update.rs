//! 回合更新请求：编辑已发出的消息、追加内容、重跑。
// 兄弟模块的类型互相引用（DaemonState 持有 EventHub、run 记录引用
// ManagerState 等），统一从 mod.rs 的再导出取，免得每个文件维护一份
// 交叉导入清单。
use super::*;
use crate::config::PromptAudience;
use crate::ipc;
use crate::state::QueuedPrompt;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::sync::Arc;

// ── TurnUpdate 请求/回执 ──
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnUpdateMode {
    Followup,
    Supersede,
}

pub(crate) struct TurnUpdateRequest {
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) session_id: Option<Arc<str>>,
    pub(crate) audience: PromptAudience,
    pub(crate) content: String,
    pub(crate) display_content: String,
    pub(crate) attachments: Vec<crate::state::QueuedPromptAttachment>,
    pub(crate) uploaded_attachment_ids: Vec<String>,
    pub(crate) mode: TurnUpdateMode,
}

pub(crate) struct TurnUpdateReceipt {
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
    pub(crate) session_id: Arc<str>,
    pub(crate) prompt: QueuedPrompt,
}

// ── enqueue_turn_update ──
pub(crate) fn enqueue_turn_update(
    state: &DaemonState,
    request: TurnUpdateRequest,
) -> Result<TurnUpdateReceipt> {
    let manager = state.manager.lock().unwrap();
    if manager.admin_busy {
        bail!("{}", ipc::ADMIN_BUSY_MESSAGE);
    }
    let run = manager
        .active_runs
        .get(&request.run_id)
        .context("active run not found")?;
    if run.audience != request.audience {
        bail!("the active reply belongs to a different request source");
    }
    if request
        .session_id
        .as_deref()
        .is_some_and(|session_id| session_id != &*run.session_id)
    {
        bail!("the active reply belongs to a different conversation");
    }
    if run.turn_id.as_deref() != Some(request.turn_id.as_str()) {
        bail!("the active run no longer owns the requested turn");
    }
    let target = run
        .queue_target
        .clone()
        .context("the active turn is not ready to accept follow-up messages")?;
    if target.turn_id != request.turn_id {
        bail!("the active run queue target changed");
    }
    let session_id = run.session_id.clone();
    let supersede = run.supersede.clone();
    let prompt_id = random_id("queued", 18);
    // 会话按人分库:排队条目要写进这条会话所在的库,否则「目标回合不在跑」
    // (成员在 AI 输出时发消息 409,09-11 实测)。
    let store = state.stores.for_session(&session_id).pinned(&session_id);
    store.recover_stale_turns()?;
    let prompt = store.enqueue_prompt_for_target_with_uploads(
        &target,
        &prompt_id,
        &request.content,
        &request.display_content,
        &request.attachments,
        &request.uploaded_attachment_ids,
    )?;
    if request.mode == TurnUpdateMode::Supersede {
        supersede.trigger();
    }
    state.events.publish(
        "queue.added",
        json!({
            "session_id": &*session_id,
            "run_id": request.run_id,
            "turn_id": request.turn_id,
            "mode": match request.mode {
                TurnUpdateMode::Followup => "followup",
                TurnUpdateMode::Supersede => "supersede",
            },
            "prompt": SafeQueuedPrompt::from(prompt.clone()),
        }),
    );
    Ok(TurnUpdateReceipt {
        run_id: request.run_id,
        turn_id: request.turn_id,
        session_id,
        prompt,
    })
}
