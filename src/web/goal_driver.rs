//! 目标续轮驱动器：会话闲下来之后，自动开下一轮把长任务往前推。
//!
//! 它是整个 goal 功能里唯一会**自己发起回合**的地方，所以每一道闸都是在回答
//! 同一个问题：「现在开这一轮，会不会踩到别人？」
//!
//! 1. 会话没有在飞的 run —— 只在空闲时驱动，不和正在跑的回合抢。
//! 2. 没有排队的人类输入 —— 人优先。自动轮消耗轮号，不该插在人前面。
//! 3. 目标是 active、本进程 armed、还有轮数余量 —— 余量耗尽就转 blocked。
//! 4. `begin_goal_round` 双 CAS 认领 —— 并发唤醒里只有一个能拿到这一轮。
//!
//! 进程内的目标状态（armed、空转等待、续轮 run 登记）都在
//! `tools::goal::runtime` 那张表里，这里只是它的消费者。
//!
//! 平台绑定的会话 v1 不驱动：回复要送达 QQ 得合成一个平台轮，那是另一套
//! 上下文装配，和这里的会话轮不是一回事。

use crate::tools::goal;
use crate::web::*;

fn event_field(record: &EventRecord, field: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&record.data)
        .ok()
        .and_then(|data| {
            data.get(field)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// 订阅事件流，在每次回合结束时看看要不要接着开一轮。
pub(in crate::web) fn spawn_goal_round_driver(state: DaemonState) {
    let mut receiver = state.events.subscribe_live();
    tokio::spawn(async move {
        loop {
            let record = match receiver.recv().await {
                Ok(record) => record,
                // 落后了就跳过：漏掉的那些回合结束事件不要紧，下一次结束
                // 还会再来，而目标本身在库里躺着不会丢。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            let session_id = || event_field(&record, "session_id");
            let run_id = || event_field(&record, "run_id").unwrap_or_default();
            match record.kind.as_str() {
                // 这一轮调过工具 = 真的在干活。判据取事件而不是回查库：
                // 驱动器本来就在订阅事件流，查库要把整个会话的回合读出来。
                // 没登记过的 run（人类轮）在 runtime 里会被直接忽略。
                "tool.started" => goal::mark_run_productive(&run_id()),
                "run.completed" => {
                    let Some(session) = session_id() else {
                        continue;
                    };
                    match goal::finish_run(&run_id()) {
                        Some((goal_session, productive)) if !productive => {
                            // 一轮下来一个工具都没调，就是在原地说话——最常见的
                            // 是「我在等你确认」。再开下去只会把同一句话重复几十
                            // 遍，每遍都收一次钱。等人开口再说。
                            tracing::info!(
                                session = %goal_session,
                                "goal round made no tool calls; pausing until the user speaks"
                            );
                            goal::set_awaiting_human(&goal_session, true);
                            continue;
                        }
                        Some(_) => {}
                        // 人（或别的来源）跑完了一轮，空转等待解除。
                        None => goal::set_awaiting_human(&session, false),
                    }
                    let state = state.clone();
                    tokio::spawn(async move {
                        maybe_continue_goal(state, session).await;
                    });
                }
                // 取消的续轮也要摘登记，不然条目在表里陪跑到进程退出。
                // 解除武装在 `cancel_run_and_disarm_goal` 里同步做了。
                "run.cancelled" => {
                    goal::finish_run(&run_id());
                    // 取消一般伴随解除武装；此刻仍是 armed，说明人在取消落地
                    // 前就 `/goal resume` 重新授权了（resume 的 nudge 撞上还没
                    // 退场的旧 run，被「会话空闲」闸拦下）。别让那次授权悬空。
                    if let Some(session) = session_id() {
                        if goal::is_armed(&session) {
                            let state = state.clone();
                            tokio::spawn(async move {
                                maybe_continue_goal(state, session).await;
                            });
                        }
                    }
                }
                "run.failed" => {
                    goal::finish_run(&run_id());
                    // 失败之后解除武装：一个会持续失败的目标如果继续自动重试，
                    // 就是拿着用户的额度撞墙。要接着跑，人得亲自 `/goal resume`。
                    if let Some(session) = session_id() {
                        if goal::is_armed(&session) {
                            tracing::info!(
                                %session,
                                "goal disarmed after a failed run; resume to continue"
                            );
                            goal::set_armed(&session, false);
                        }
                    }
                }
                _ => {}
            }
        }
    });
}

/// `/goal` 命令的 daemon 侧总入口：执行命令，然后做界面层做不到的两件事。
///
/// - **pause/clear 要立刻停住正在跑的续轮**。命令本身只关下一轮的闸
///   （disarm），可用户按下暂停时看到的是「它还在跑」——状态行都关了输出还在
///   滚，只能再去点停止按钮。
/// - **edit 要在步间到达正在跑的续轮**。不通知的话，模型要么拿着旧 revision
///   撞一次 CAS 才发现世界变了，要么整轮都在推进旧目标。
///
/// REPL 走 IPC、WebUI 走 HTTP，两条路都必须从这里过：各写一份迟早分叉。
pub(in crate::web) fn apply_goal_command(
    state: &DaemonState,
    session_id: &str,
    input: &str,
) -> String {
    let verb = input.split_whitespace().next().unwrap_or("");
    let result = goal::try_execute_goal_command(&state.paths, session_id, input);
    let succeeded = result.is_ok();
    let text = match result {
        Ok(text) => text,
        Err(error) => format!("{error}"),
    };
    match verb {
        // 命令失败也取消：失败说明目标状态已经不是「可暂停」了（比如已经
        // blocked），但用户的意图仍是「停下」，正在飞的续轮没有理由继续。
        "pause" | "clear" => cancel_goal_rounds_for_session(state, session_id),
        // edit 的语义是「掐掉旧目标的这一轮，按新目标重来」：取消时 armed
        // 保持为真，`run.cancelled` 处理里看到仍在武装就会立刻重开一轮，
        // 新一轮的提示词自带新目标。不搞任何「变更通知」注入——中断和
        // 重启本身就是用户能看见的反馈。
        "edit" if succeeded => cancel_goal_rounds_for_session(state, session_id),
        _ => {}
    }
    // 人刚动过目标，驱动器该重新看一眼：`/goal resume` 之后正是要它立刻接着
    // 跑，而不是干等下一个回合结束事件——那个事件可能永远不来（会话正闲着）。
    nudge_goal_driver(state, session_id);
    text
}

/// 取消这个会话所有在飞的续轮 run。人类发起的回合不碰。
fn cancel_goal_rounds_for_session(state: &DaemonState, session_id: &str) {
    let manager = state.manager.lock().unwrap();
    for run in manager.active_runs.values() {
        if &*run.session_id == session_id
            && matches!(
                run.turn_origin,
                crate::tools::workspace::TurnOrigin::GoalRound { .. }
            )
        {
            run.request_cancel();
        }
    }
}

fn session_has_goal_round_run(state: &DaemonState, session_id: &str) -> bool {
    let manager = state.manager.lock().unwrap();
    manager.active_runs.values().any(|run| {
        &*run.session_id == session_id
            && matches!(
                run.turn_origin,
                crate::tools::workspace::TurnOrigin::GoalRound { .. }
            )
    })
}

/// 人刚动过目标，踢驱动器一脚。重复踢是安全的：四道闸都在
/// `maybe_continue_goal` 里，多踢几次最多是多查几次库。
pub(in crate::web) fn nudge_goal_driver(state: &DaemonState, session_id: &str) {
    // 人动过目标就是开了口：上一轮空转设下的等待到此为止。
    goal::set_awaiting_human(session_id, false);
    if !goal::is_armed(session_id) {
        return;
    }
    let state = state.clone();
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        maybe_continue_goal(state, session_id).await;
    });
}

/// 四道闸之后认领下一轮，起一个自动回合。
pub(in crate::web) async fn maybe_continue_goal(state: DaemonState, session_id: String) {
    // 闸 0：上一轮空转过就等人。
    if goal::is_awaiting_human(&session_id) {
        return;
    }
    // 闸 1：会话空闲。
    {
        let manager = state.manager.lock().unwrap();
        if manager.admin_busy
            || manager
                .active_runs
                .values()
                .any(|run| &*run.session_id == session_id.as_str())
        {
            return;
        }
    }
    // 闸 2：人在排队就让行。自动轮会消耗轮号，插在人前面既抢了额度也抢了顺序。
    let store = state.state_store.pinned(&session_id);
    match store.load_queued_prompts() {
        Ok(queued) if queued.is_empty() => {}
        _ => return,
    }
    let Ok(Some(record)) = state.state_store.session_record(&session_id) else {
        return;
    };
    // 平台绑定会话 v1 不驱动（回复送达要合成平台轮，与这里的会话轮不同构）。
    if let Ok(bindings) = state
        .state_store
        .platform_session_bindings(&record.persona, "onebot")
    {
        if bindings
            .iter()
            .any(|binding| binding.session_id == session_id)
        {
            tracing::debug!(%session_id, "goal driver skips platform-bound session (v1)");
            return;
        }
    }
    // 闸 3：目标可跑。
    let Ok(Some(goal_record)) = state.state_store.goal(&session_id) else {
        return;
    };
    if goal_record.phase != crate::state::GoalPhase::Active || !goal::is_armed(&session_id) {
        return;
    }
    if goal_record.rounds_started >= goal_record.max_rounds {
        // 轮数耗尽转 blocked 而不是静默停下：人得知道它为什么不动了，
        // 以及该怎么继续（/goal edit 抬高上限）。
        let _ = state.state_store.block_goal(
            &session_id,
            &goal_record.goal_id,
            goal_record.revision,
            "round-limit",
            &format!(
                "goal reached its configured limit of {} rounds",
                goal_record.max_rounds
            ),
        );
        goal::set_armed(&session_id, false);
        return;
    }
    // 闸 4：双 CAS 认领。并发唤醒里只有一个能赢，输的那个安静退出。
    let claimed = match state.state_store.begin_goal_round(
        &session_id,
        &goal_record.goal_id,
        goal_record.revision,
        goal_record.rounds_started,
    ) {
        Ok(claimed) => claimed,
        Err(error) => {
            tracing::debug!(%session_id, error = %error, "goal round claim lost; not continuing");
            return;
        }
    };

    // 第一轮发完整版（目标、规矩、来历、收尾方式）；之后逐轮发自包含的短版。
    // 短版自己带来历和目标全文，所以压缩、目标被改这些事都不需要专门补救。
    let content = goal::goal_round_prompt(&claimed, claimed.rounds_started <= 1);
    // 前缀是给前端认的：续轮在时间线里**什么都不画**（状态行已经在说
    // 「进行中 · 第 N 轮」了，对话流里再来一条只是噪声）。文案本身只在
    // 日志和调试时看得到。改这个前缀要同时改 `web/app.js` 的
    // `isSyntheticTurnContent`、`src/cli/repl/jobs.rs`、`src/cli/mod.rs`
    // 的回放分支和 `conversation_db` 的 SQL LIKE。
    let display_content = format!(
        "[目标续轮] 第 {} 轮 · {}",
        claimed.rounds_started, claimed.objective
    );
    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    // 带上认领时的身份：工具层据此判定「本轮恰好是当前目标的这一轮」，
    // 只有这样才允许模型自己报完成/受阻。
    let origin = crate::tools::workspace::TurnOrigin::GoalRound {
        goal_id: claimed.goal_id.clone(),
        revision: claimed.revision,
        round: claimed.rounds_started,
    };
    {
        let mut manager = state.manager.lock().unwrap();
        // 认领和登记之间可能有人抢了管理锁，这里再看一眼。
        if manager.admin_busy {
            return;
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone().into(),
                mode: AgentMode::Normal,
                audience: PromptAudience::Owner,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                // 复用 job_wake 这条可见性通道：REPL 客户端靠它发现并挂上
                // daemon 自己发起的回合，否则自动轮在终端里是完全看不见的。
                job_wake: true,
                turn_origin: origin.clone(),
                job_wake_label: Some(goal::GOAL_ROUND_LABEL.to_string()),
            },
        );
    }
    goal::track_goal_run(&run_id, &session_id);
    tracing::info!(
        %session_id,
        round = claimed.rounds_started,
        max = claimed.max_rounds,
        "goal driver starting an autonomous round"
    );
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id: session_id.clone().into(),
            content,
            display_content,
            attachment_run_id: None,
            mode: AgentMode::Normal,
            images: Vec::new(),
            cwd: None,
            origin_tty: None,
            audience: PromptAudience::Owner,
            profile: None,
            overrides: None,
            cancel: cancel_rx,
            turn_origin: Box::new(origin),
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
    }
}
