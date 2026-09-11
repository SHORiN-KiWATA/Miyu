//! 后台任务完成后的唤醒。
//!
//! 任务跑完要把结论送到用户面前，但「面前」有三种：网页（走事件流）、发起它的
//! 终端（`stream_job_wake_to_origin_tty`）、平台会话（`wake_platform_session_for_job`）。
//!
//! 写回终端前要确认它还在、还停在提示符处——正在跑别的命令时插一段输出会把人家
//! 的界面搅乱。

use crate::web::*;

/// Background-job completions wake the model so it can follow up on the
/// result autonomously. Local sessions get a real turn (or a queued
/// followup when the session is mid-turn); platform-bound sessions get a
/// plain-text broadcast into the conversation — a self-initiated platform
/// turn would need synthetic sender semantics the plugins aren't built for.
/// goal 续轮驱动器(任务#10,dsh goal-round-driver 的 daemon 化)。
/// 订阅 run 生命周期事件,在会话空闲检查点推进 armed 的 active 目标:
/// - run.completed → 尝试认领下一轮(四道栅栏见 maybe_continue_goal)
/// - run.failed → disarm(异常不自动重试,dsh 同款;等人 resume)
/// 取消→pause 的语义在 ipc Cancel 处理器里(那里能拿到被取消 run 的来源)。
pub(in crate::web) fn install_background_job_hook(state: &DaemonState) {
    let started_state = state.clone();
    tools::jobs::set_started_hook(Arc::new(move |overview| {
        started_state
            .events
            .publish("job.started", json!({ "job": overview }));
    }));
    // 后台子代理的实时进度上 SSE:网页端据 job_id 把它渲进任务条那个任务的
    // 子过程流(点开后台子代理即可看流式,与前台子代理工具行同款)。
    let progress_state = state.clone();
    tools::jobs::set_progress_hook(Arc::new(move |job_id, message| {
        progress_state.events.publish(
            "job.progress",
            json!({ "job_id": job_id, "message": message }),
        );
    }));
    let hook_state = state.clone();
    tools::jobs::set_completion_hook(Arc::new(move |completion| {
        let state = hook_state.clone();
        tokio::spawn(async move {
            handle_job_completion(state, completion).await;
        });
    }));
}

pub(in crate::web) async fn handle_job_completion(
    state: DaemonState,
    completion: tools::jobs::JobCompletion,
) {
    state.events.publish(
        "job.finished",
        json!({
            "job_id": completion.job_id,
            "title": completion.title,
            "status": completion.state_label,
            "runtime_seconds": completion.runtime_seconds,
        }),
    );
    tracing::info!(
        job_id = %completion.job_id,
        wake_requested = completion.wake_requested,
        has_session = completion.session_id.is_some(),
        has_origin_tty = completion.origin_tty.is_some(),
        "background job finished"
    );
    if !completion.wake_requested {
        // The model stopped this command itself; clean the strips quietly.
        tools::jobs::acknowledge(&completion.job_id);
        state
            .events
            .publish("job.acknowledged", json!({ "job_id": completion.job_id }));
        return;
    }
    let command_short = completion.command.chars().take(120).collect::<String>();
    let mut pending_wake_run: Option<JobWakeRun> = None;
    if let Some(session_id) = completion.session_id.clone() {
        match state.state_store.is_platform_session(&session_id) {
            Ok(true) => {
                wake_platform_session_for_job(&state, &session_id, &completion).await;
            }
            Ok(false) => {
                pending_wake_run =
                    wake_local_session_for_job(&state, session_id, &completion, &command_short);
            }
            Err(error) => {
                tracing::warn!(
                    job_id = %completion.job_id,
                    error = %error,
                    "failed to resolve the session of a finished background command"
                );
            }
        }
    }
    // Keep the finished job visible in UI strips until its wake turn is done
    // (the report is what replaces the strip line); everything else clears
    // right away.
    if let Some(wake) = pending_wake_run {
        // 流式回写与等待循环并行:回合一开跑就把思考/工具/正文追加进触发
        // 终端,acknowledge 只关心回合何时结束。
        if completion.origin_tty.is_some() {
            let stream_state = state.clone();
            let stream_completion = completion.clone();
            let stream_wake = wake.clone();
            tokio::spawn(async move {
                stream_job_wake_to_origin_tty(stream_state, stream_completion, stream_wake).await;
            });
        }
        // 事件驱动：run 结束由 finish_run 的 runs_changed 通知，不再
        // 500ms 拿全局锁轮询。notified() 在查条件**之前**注册，堵死
        // 「查完没在等、通知恰好落空」的竞态；60s 慢速兜底纯属防御。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        let notify = state.manager.lock().unwrap().runs_changed.clone();
        loop {
            let notified = notify.notified();
            let still_running = state
                .manager
                .lock()
                .unwrap()
                .active_runs
                .contains_key(&wake.run_id);
            if !still_running || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => {}
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
        }
    }
    tools::jobs::acknowledge(&completion.job_id);
    state
        .events
        .publish("job.acknowledged", json!({ "job_id": completion.job_id }));
}

/// 本地会话唤醒回合的标识:run id + 事件订阅起点(在回合入队前取,保证
/// 从 turn.started 起一帧不漏)。
#[derive(Clone)]
pub(in crate::web) struct JobWakeRun {
    pub(in crate::web) run_id: String,
    pub(in crate::web) events_after: u64,
}

/// 把唤醒回合流式渲染进当初触发 shellhook/单次 CLI 的终端:思考(暗色,按
/// display.reasoning 配置)、工具行、正文逐行 Markdown。触发端进程早已退出,
/// 由 daemon 直接写 tty 设备。三道闸全过才动笔:
/// 1. `notifications.job_writeback_to_terminal` 开关(默认开);
/// 2. 触发 shell 还活着且 stdin 仍指向记录的 tty——终端关闭、pid 复用都拦下;
/// 3. shell 空闲在前台提示符(tpgid==pgrp)——正开着 vim/htop 时绝不能撕屏。
/// 追加式输出,无光标控制;每次落笔前重查第 3 道闸,中途被占立即收笔并补
/// 桌面通知。物理写入走专职线程,^S 流控卡死也只占一根线程。
pub(in crate::web) async fn stream_job_wake_to_origin_tty(
    state: DaemonState,
    completion: tools::jobs::JobCompletion,
    wake: JobWakeRun,
) {
    let Some(origin) = completion.origin_tty.clone() else {
        return;
    };
    let config = crate::config::AppConfig::load_or_default(&state.paths).unwrap_or_default();
    if !config.notifications.job_writeback_to_terminal {
        return;
    }
    let notify_fallback = |reason: &str| {
        tracing::info!(job_id = %completion.job_id, reason, "job wake writeback fell back to a notification");
        if config.notifications.enabled {
            crate::notify::notify(
                &format!("Miyu 后台任务跟进 · {}", completion.title),
                "任务已完成,跟进回复在会话里(终端不在提示符,没有直接写入)。",
            );
        }
    };
    if !origin_shell_at_prompt(&origin) {
        notify_fallback("shell not at prompt");
        return;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let tty = match std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(&origin.path)
    {
        Ok(tty) => tty,
        Err(error) => {
            tracing::debug!(job_id = %completion.job_id, %error, "origin tty open failed");
            notify_fallback("tty open failed");
            return;
        }
    };
    tracing::info!(
        job_id = %completion.job_id,
        run_id = %wake.run_id,
        tty = %origin.path.display(),
        shell_pid = origin.shell_pid,
        "streaming job wake reply to the originating terminal"
    );

    let (ops_tx, ops_rx) = std::sync::mpsc::channel::<TtyWriteOp>();
    let shell_pid = origin.shell_pid;
    let writer = std::thread::Builder::new()
        .name("miyu-tty-writeback".to_string())
        .spawn(move || origin_tty_writer(tty, shell_pid, ops_rx));
    if writer.is_err() {
        notify_fallback("writer thread spawn failed");
        return;
    }

    let reasoning_mode =
        crate::render::ReasoningDisplayMode::from_config(&config.display.reasoning);
    // 落笔即有反馈:头部先行,正文随事件到达逐行追加。
    let _ = ops_tx.send(TtyWriteOp::Write(format!(
        "\r\n\x1b[1m✦ Miyu 后台任务跟进\x1b[0m \x1b[2m· {}\x1b[0m\r\n\r\n",
        completion.title
    )));

    let mut subscription = state.events.subscribe_after(wake.events_after);
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut reasoning_buf = String::new();
    let mut content_buf = String::new();
    let mut wrote_reasoning = false;
    let mut reasoning_open = false;
    let mut last_id = wake.events_after;
    let mut aborted = false;
    loop {
        if std::time::Instant::now() > deadline {
            aborted = true;
            break;
        }
        let record = if let Some(record) = subscription.pending.pop_front() {
            record
        } else {
            match tokio::time::timeout(Duration::from_secs(30), subscription.receiver.recv()).await
            {
                Ok(Ok(record)) => record,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    subscription.pending = state.events.replay_after(last_id);
                    continue;
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => break,
                Err(_) => {
                    // 静默期顺手确认回合还活着,免得错过终态事件后干等。
                    if !state
                        .manager
                        .lock()
                        .unwrap()
                        .active_runs
                        .contains_key(&wake.run_id)
                    {
                        break;
                    }
                    continue;
                }
            }
        };
        last_id = record.id;
        let Ok(data) = serde_json::from_str::<Value>(&record.data) else {
            continue;
        };
        if data.get("run_id").and_then(Value::as_str) != Some(wake.run_id.as_str()) {
            if !state
                .manager
                .lock()
                .unwrap()
                .active_runs
                .contains_key(&wake.run_id)
            {
                break;
            }
            continue;
        }

        let mut chunk_out = String::new();
        match record.kind.as_str() {
            "reasoning.title" => {
                if matches!(reasoning_mode, crate::render::ReasoningDisplayMode::Summary) {
                    if let Some(title) = data.get("title").and_then(Value::as_str) {
                        flush_line_buf(
                            &mut reasoning_buf,
                            WriteLineStyle::Reasoning,
                            &mut chunk_out,
                        );
                        push_rendered_line(
                            &format!("∴ {title}"),
                            WriteLineStyle::Reasoning,
                            &mut chunk_out,
                        );
                        wrote_reasoning = true;
                        reasoning_open = true;
                    }
                }
            }
            "reasoning.delta" => {
                if matches!(reasoning_mode, crate::render::ReasoningDisplayMode::Full) {
                    if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                        reasoning_buf.push_str(delta);
                        drain_line_buf(
                            &mut reasoning_buf,
                            WriteLineStyle::Reasoning,
                            &mut chunk_out,
                        );
                        wrote_reasoning = true;
                        reasoning_open = true;
                    }
                }
            }
            "reasoning.part_end" | "reasoning.reset" => {
                flush_line_buf(
                    &mut reasoning_buf,
                    WriteLineStyle::Reasoning,
                    &mut chunk_out,
                );
            }
            "tool.started" => {
                flush_line_buf(
                    &mut reasoning_buf,
                    WriteLineStyle::Reasoning,
                    &mut chunk_out,
                );
                if reasoning_open {
                    chunk_out.push_str("\r\n");
                    reasoning_open = false;
                }
                let name = data
                    .get("display_name")
                    .or_else(|| data.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("工具");
                push_rendered_line(&format!("⚙ {name} …"), WriteLineStyle::Note, &mut chunk_out);
            }
            "tool.finished" => {
                if data.get("ok").and_then(Value::as_bool) == Some(false) {
                    let name = data
                        .get("display_name")
                        .or_else(|| data.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or("工具");
                    push_rendered_line(
                        &format!("⚙ {name} 失败"),
                        WriteLineStyle::Note,
                        &mut chunk_out,
                    );
                }
            }
            "assistant.delta" => {
                flush_line_buf(
                    &mut reasoning_buf,
                    WriteLineStyle::Reasoning,
                    &mut chunk_out,
                );
                if reasoning_open
                    || (wrote_reasoning && content_buf.is_empty() && chunk_out.is_empty())
                {
                    chunk_out.push_str("\r\n");
                    reasoning_open = false;
                    wrote_reasoning = false;
                }
                if let Some(delta) = data.get("delta").and_then(Value::as_str) {
                    content_buf.push_str(delta);
                    drain_line_buf(&mut content_buf, WriteLineStyle::Content, &mut chunk_out);
                }
            }
            "run.completed" | "run.failed" | "run.cancelled" => {
                flush_line_buf(
                    &mut reasoning_buf,
                    WriteLineStyle::Reasoning,
                    &mut chunk_out,
                );
                flush_line_buf(&mut content_buf, WriteLineStyle::Content, &mut chunk_out);
                if record.kind != "run.completed" {
                    push_rendered_line("(跟进中断)", WriteLineStyle::Note, &mut chunk_out);
                }
                // fish/zsh 收到 SIGWINCH 重绘提示符时,会从光标行向上清掉
                // 自家提示符高度的行数再画(starship 双行提示符实测清 2 行)。
                // 垫两行空白当牺牲品,免得清到正文末行。
                chunk_out.push_str("\r\n\r\n\r\n");
                let _ = ops_tx.send(TtyWriteOp::Write(chunk_out));
                let _ = ops_tx.send(TtyWriteOp::Finish);
                tracing::info!(
                    job_id = %completion.job_id,
                    outcome = %record.kind,
                    "job wake reply streamed to the originating terminal"
                );
                return;
            }
            _ => {}
        }
        if !chunk_out.is_empty() {
            // 落笔前重查前台闸:用户开了全屏程序就立即收笔,已写的留在屏上。
            if !origin_shell_at_prompt(&origin) {
                aborted = true;
                break;
            }
            let _ = ops_tx.send(TtyWriteOp::Write(chunk_out));
        }
    }
    let _ = ops_tx.send(TtyWriteOp::Abort);
    if aborted {
        notify_fallback("interrupted mid-stream");
    }
}

/// 专职写线程:tty 是同步阻塞设备(^S 流控可以永久卡住 write),隔离在自己
/// 的线程里,卡死也只占一根线程,不拖累 daemon 的 async runtime。
pub(in crate::web) fn origin_tty_writer(
    mut tty: std::fs::File,
    shell_pid: u32,
    ops: std::sync::mpsc::Receiver<TtyWriteOp>,
) {
    use std::io::Write;
    for op in ops {
        match op {
            TtyWriteOp::Write(text) => {
                if tty.write_all(text.as_bytes()).is_err() {
                    return;
                }
            }
            TtyWriteOp::Finish => {
                let _ = tty.flush();
                // 提示符被我们的输出推到半空,SIGWINCH 让 shell(fish/zsh/新
                // bash 的 readline 都处理)原地重绘一行干净的提示符。
                unsafe {
                    libc::kill(shell_pid as i32, libc::SIGWINCH);
                }
                return;
            }
            TtyWriteOp::Abort => return,
        }
    }
}

pub(in crate::web) fn wake_local_session_for_job(
    state: &DaemonState,
    session_id: Arc<str>,
    completion: &tools::jobs::JobCompletion,
    command_short: &str,
) -> Option<JobWakeRun> {
    let noun = if completion.is_subagent {
        "后台子代理"
    } else {
        "后台命令"
    };
    // 结果直接附在唤醒里,不再让模型「先去查一次再汇报」。子代理给完整结论
    // (它就是交付物),命令给日志尾部;剩下的自己判断——只给事实和日志路径,
    // 不给动作指示。
    let result_block = tools::jobs::completion_result(
        &completion.log_path,
        completion.is_subagent,
        completion.exit_code == Some(0),
    )
    .map(|(label, body)| format!("- {label}:\n{body}\n"))
    .unwrap_or_default();
    let content = format!(
        "<background-job-report>{noun}「{}」已执行完毕：\n\
         - job_id: {}\n- 任务: {}\n- 状态: {}（运行 {} 秒）\n\
         - 日志: {}\n{result_block}\
         这是系统自动触发的跟进，不是用户消息。\
         </background-job-report>",
        completion.title,
        completion.job_id,
        command_short,
        completion.state_label,
        completion.runtime_seconds,
        completion.log_path.display(),
    );
    let display_content = format!(
        "[后台任务完成] {}完成 {} · {}",
        if completion.is_subagent {
            "子代理"
        } else {
            "命令"
        },
        completion.job_id,
        completion.title
    );

    // Mid-turn session: ride the queue so the model reacts within the
    // running reply instead of colliding with it.
    let queued = {
        let manager = state.manager.lock().unwrap();
        manager
            .active_runs
            .iter()
            .find(|(_, info)| &*info.session_id == &*session_id)
            .map(|(run_id, info)| (run_id.clone(), info.queue_target.clone(), info.audience))
    };
    if let Some((run_id, queue_target, audience)) = queued {
        tracing::info!(
            job_id = %completion.job_id,
            run_id = %run_id,
            has_queue_target = queue_target.is_some(),
            "job wake joining the session's active run"
        );
        let Some(target) = queue_target else {
            // Turn is still starting; report on the next completion poll
            // rather than racing its queue setup.
            tracing::debug!(job_id = %completion.job_id, "job wake skipped: turn starting");
            return None;
        };
        let request = TurnUpdateRequest {
            run_id,
            turn_id: target.turn_id,
            session_id: Some(session_id.clone()),
            audience,
            content,
            display_content,
            attachments: Vec::new(),
            uploaded_attachment_ids: Vec::new(),
            mode: TurnUpdateMode::Followup,
        };
        if let Err(error) = enqueue_turn_update(state, request) {
            tracing::debug!(
                job_id = %completion.job_id,
                error = %error,
                "job wake could not join the running turn"
            );
        }
        return None;
    }

    let run_id = random_id("run", 18);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    {
        let mut manager = state.manager.lock().unwrap();
        if manager.admin_blocks_session(&session_id) {
            tracing::debug!(job_id = %completion.job_id, "job wake skipped: admin busy");
            return None;
        }
        manager.active_runs.insert(
            run_id.clone(),
            RunInfo {
                session_id: session_id.clone(),
                mode: AgentMode::Normal,
                audience: PromptAudience::Owner,
                cancel: cancel_tx,
                turn_id: None,
                queue_target: None,
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: true,
                turn_origin: crate::tools::workspace::TurnOrigin::JobWake,
                job_wake_label: Some(format!(
                    "{}完成 {} · {}",
                    if completion.is_subagent {
                        "子代理"
                    } else {
                        "命令"
                    },
                    completion.job_id,
                    completion.title
                )),
            },
        );
    }
    // 订阅起点在入队前取:回合的 turn.started 起所有事件都不漏给流式回写。
    let events_after = state.events.latest_id();
    if state
        .actor_tx
        .send(ActorCommand::StartTurn {
            run_id: run_id.clone(),
            session_id,
            content,
            display_content,
            attachment_run_id: None,
            mode: AgentMode::Normal,
            images: Vec::new(),
            cwd: Some(completion.workspace.clone()),
            origin_tty: completion.origin_tty.clone().map(Box::new),
            audience: PromptAudience::Owner,
            profile: None,
            overrides: None,
            cancel: cancel_rx,
            turn_origin: Box::new(crate::tools::workspace::TurnOrigin::JobWake),
        })
        .is_err()
    {
        finish_run(&state.manager, &run_id, None);
        return None;
    }
    Some(JobWakeRun {
        run_id,
        events_after,
    })
}

pub(in crate::web) async fn wake_platform_session_for_job(
    state: &DaemonState,
    session_id: &Arc<str>,
    completion: &tools::jobs::JobCompletion,
) {
    let persona = state.manager.lock().unwrap().config.active_persona_scope();
    let binding = state
        .state_store
        .platform_session_bindings(&persona, "onebot")
        .ok()
        .and_then(|bindings| {
            bindings
                .into_iter()
                .find(|binding| binding.session_id == **session_id)
        });
    let Some(binding) = binding else {
        tracing::debug!(job_id = %completion.job_id, "job wake skipped: no platform binding");
        return;
    };
    let noun = if completion.is_subagent {
        "后台子代理"
    } else {
        "后台命令"
    };
    // 与本地唤醒同款:结果直接附在唤醒里(子代理给完整结论,命令给日志尾部),
    // 只给事实,不再指示模型「先去查一次再汇报」。
    let result_block = tools::jobs::completion_result(
        &completion.log_path,
        completion.is_subagent,
        completion.exit_code == Some(0),
    )
    .map(|(label, body)| format!("- {label}:\n{body}\n"))
    .unwrap_or_default();
    let content = format!(
        "<background-job-report>{noun}「{}」已执行完毕：\n- job_id: {}\n- 任务: {}\n- 状态: {}（运行 {} 秒）\n\
         {result_block}这是系统自动触发的跟进，不是用户消息。\
         </background-job-report>",
        completion.title,
        completion.job_id,
        completion.command.chars().take(200).collect::<String>(),
        completion.state_label,
        completion.runtime_seconds
    );
    if let Err(error) = crate::platforms::onebot::wake_conversation_for_job(
        state,
        &binding.key.account_id,
        &binding.key.conversation_kind,
        &binding.key.conversation_id,
        completion.platform_sender.as_deref(),
        content,
    )
    .await
    {
        tracing::warn!(
            job_id = %completion.job_id,
            error = %error,
            "failed to wake the model for a background command in QQ"
        );
    }
}
