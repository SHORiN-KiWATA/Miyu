//! 回合的创建、更新、重跑与队列。

use super::shared::*;
use crate::question::QuestionResponse;
use crate::web::*;

#[test]
fn system_prompt_resource_is_not_exposed_as_a_persona_document() {
    let temp = tempfile::tempdir().unwrap();
    let mut paths = test_paths(temp.path());
    paths.skills_dir = paths.data_dir.join("skills");
    let prompts = paths.prompts_dir();
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("system-prompt.md"), "fallback").unwrap();
    std::fs::write(prompts.join("Persona.md"), "persona").unwrap();

    let documents = read_prompt_documents(&AppConfig::default(), &paths).unwrap();
    assert_eq!(documents.personas.len(), 1);
    assert_eq!(documents.personas[0].name, "Persona.md");
}

#[test]
fn actor_commands_keep_large_configuration_off_the_inline_queue_item() {
    assert!(std::mem::size_of::<ActorCommand>() <= 512);
}

#[test]
fn prompt_sidecar_reads_avatar_path_without_touching_prompt_content() {
    let temp = tempfile::tempdir().unwrap();
    let prompt = temp.path().join("Alice.md");
    std::fs::write(&prompt, "You are Alice.\n").unwrap();
    std::fs::write(
        temp.path().join("Alice.json"),
        r#"{"avatar_path":"avatars/alice.png","board_image_path":"persona-avatars/board.png","board_title":"欢迎","board_subtitle":"从这里开始","starter_prompts":["天气","问题"]}"#,
    )
    .unwrap();

    let documents = read_prompt_document_dir(temp.path(), true).unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].name, "Alice.md");
    assert_eq!(documents[0].content, "You are Alice.\n");
    assert_eq!(
        documents[0].avatar_path.as_deref(),
        Some("avatars/alice.png")
    );
    assert_eq!(
        documents[0].board_image_path.as_deref(),
        Some("persona-avatars/board.png")
    );
    assert_eq!(documents[0].board_title.as_deref(), Some("欢迎"));
    assert_eq!(documents[0].starter_prompts.as_ref().map(Vec::len), Some(2));
}

#[test]
fn malformed_prompt_sidecar_falls_back_to_no_avatar() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("Alice.md"), "prompt").unwrap();
    std::fs::write(temp.path().join("Alice.json"), "not json").unwrap();

    let documents = read_prompt_document_dir(temp.path(), true).unwrap();
    assert_eq!(documents[0].avatar_path, None);
}

#[test]
fn active_turn_queue_never_crosses_prompt_audiences() {
    let (manager, _cancel_rx) = manager_with_run("owner_run");
    let manager = manager.lock().unwrap();

    assert!(manager.session_runs_match_audience("default", PromptAudience::Owner));
    assert!(!manager.session_runs_match_audience("default", PromptAudience::External));
    assert!(!manager.session_runs_match_audience("missing", PromptAudience::Owner));
}

#[test]
fn light_admin_reservation_allows_running_turns_and_serializes_mutations() {
    let (manager, _cancel_rx) = manager_with_run("active_run");

    assert!(reserve_admin(&manager).is_err());
    assert!(reserve_admin_light(&manager).is_ok());
    assert!(reserve_admin_light(&manager).is_err());
    assert_eq!(manager.lock().unwrap().active_runs.len(), 1);

    release_admin(&manager);
    assert!(reserve_admin_light(&manager).is_ok());
    release_admin(&manager);
}

/// 把一条「跑着的轮」塞进 manager，返回它的 store（队列目标从这里来）。
fn state_with_running_run(
    state: &DaemonState,
    run_id: &str,
    turn_id: &str,
    audience: PromptAudience,
    turn_origin: crate::tools::workspace::TurnOrigin,
) -> (StateStore, tokio::sync::watch::Receiver<bool>) {
    let session_id = state.state_store.session_id();
    let store = state.state_store.pinned_for_turn(&session_id);
    store
        .start_turn(turn_id, turn_id, std::process::id())
        .unwrap();
    let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
    state.manager.lock().unwrap().active_runs.insert(
        run_id.to_string(),
        RunInfo {
            session_id,
            mode: AgentMode::Normal,
            audience,
            cancel,
            turn_id: Some(turn_id.to_string()),
            queue_target: Some(store.queue_target(turn_id)),
            supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
            platform_followup: None,
            operation: RunOperation::Create,
            job_wake: true,
            turn_origin,
            job_wake_label: None,
        },
    );
    (store, cancel_rx)
}

/// 目标续轮是机器自己开的 `Owner` 轮，WebUI 发的消息要排得进去——修复前排队
/// 请求写死 `External`，被 `enqueue_turn_update` 的 audience 校验挡成 409，
/// 前端弹「这条没发出去：会话刚开始新的一轮」。
#[test]
fn webui_followups_queue_into_a_running_goal_round() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let session_id = state.state_store.session_id();
    let (store, _cancel_rx) = state_with_running_run(
        &state,
        "run-goal",
        "turn-goal",
        PromptAudience::Owner,
        crate::tools::workspace::TurnOrigin::GoalRound {
            goal_id: "g".into(),
            revision: 1,
            round: 1,
        },
    );

    let receipt = queue_into_running_session(&state, &session_id, "插一句", "插一句", &[])
        .expect("queueing into a goal round must not be a 409")
        .expect("the goal round is queueable");
    assert_eq!(receipt.run_id, "run-goal");
    assert_eq!(receipt.turn_id, "turn-goal");
    assert_eq!(store.load_queued_prompts().unwrap().len(), 1);

    let manager = state.manager.lock().unwrap();
    assert_eq!(
        web_followup_audience(manager.active_runs.get("run-goal").unwrap()),
        PromptAudience::Owner
    );
}

/// 反面：REPL 起的轮（`Owner` + `Human`）WebUI 仍旧排不进去，`create_turn`
/// 顺着 `Ok(None)` 走到起新轮那条路，在 `session_has_runs` 上拿 409「Miyu is
/// busy」。跨端隔离不能被上一条顺手拆掉。
#[test]
fn webui_followups_still_stay_out_of_repl_turns() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let session_id = state.state_store.session_id();
    let (store, _cancel_rx) = state_with_running_run(
        &state,
        "run-repl",
        "turn-repl",
        PromptAudience::Owner,
        crate::tools::workspace::TurnOrigin::Human,
    );

    assert!(
        queue_into_running_session(&state, &session_id, "插一句", "插一句", &[])
            .unwrap()
            .is_none()
    );
    assert!(store.load_queued_prompts().unwrap().is_empty());

    let manager = state.manager.lock().unwrap();
    assert_eq!(
        web_followup_audience(manager.active_runs.get("run-repl").unwrap()),
        PromptAudience::External
    );
    assert!(manager.session_has_runs(&session_id));
}

#[test]
fn turn_updates_are_routed_to_the_exact_run_and_turn() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();
    let session_id = state.state_store.session_id();
    let first_store = state.state_store.pinned_for_turn(&session_id);
    let second_store = state.state_store.pinned_for_turn(&session_id);
    first_store
        .start_turn("turn-first", "first", std::process::id())
        .unwrap();
    second_store
        .start_turn("turn-second", "second", std::process::id())
        .unwrap();
    let mut manager = state.manager.lock().unwrap();
    for (run_id, turn_id, store) in [
        ("run-first", "turn-first", &first_store),
        ("run-second", "turn-second", &second_store),
    ] {
        let (cancel, _cancel_rx) = tokio::sync::watch::channel(false);
        manager.active_runs.insert(
            run_id.to_string(),
            RunInfo {
                session_id: session_id.clone(),
                mode: AgentMode::Normal,
                audience: PromptAudience::External,
                cancel,
                turn_id: Some(turn_id.to_string()),
                queue_target: Some(store.queue_target(turn_id)),
                supersede: Arc::new(crate::agent::TurnSupersedeSignal::default()),
                platform_followup: None,
                operation: RunOperation::Create,
                job_wake: false,
                turn_origin: crate::tools::workspace::TurnOrigin::Human,
                job_wake_label: None,
            },
        );
    }
    drop(manager);

    enqueue_turn_update(
        &state,
        TurnUpdateRequest {
            run_id: "run-first".to_string(),
            turn_id: "turn-first".to_string(),
            session_id: Some(session_id.clone()),
            audience: PromptAudience::External,
            content: "follow first".to_string(),
            display_content: "follow first".to_string(),
            attachments: Vec::new(),
            uploaded_attachment_ids: Vec::new(),
            mode: TurnUpdateMode::Followup,
        },
    )
    .unwrap();

    assert_eq!(first_store.load_queued_prompts().unwrap().len(), 1);
    assert!(second_store.load_queued_prompts().unwrap().is_empty());
    assert!(enqueue_turn_update(
        &state,
        TurnUpdateRequest {
            run_id: "run-first".to_string(),
            turn_id: "turn-second".to_string(),
            session_id: Some(session_id),
            audience: PromptAudience::External,
            content: "wrong target".to_string(),
            display_content: "wrong target".to_string(),
            attachments: Vec::new(),
            uploaded_attachment_ids: Vec::new(),
            mode: TurnUpdateMode::Followup,
        },
    )
    .is_err());
}

#[test]
fn dropped_ipc_turn_detaches_without_cancelling_the_run() {
    // dsh 语义:前端断线,回合继续——guard 掉落绝不发取消。
    let (manager, cancel_rx) = manager_with_run("run_test");
    drop(IpcRunGuard {
        manager,
        run_id: "run_test".to_string(),
        finished: false,
        one_shot: false,
        questions: None,
    });
    assert!(!*cancel_rx.borrow());
}

/// 一次性客户端(shellhook/单次)断线是例外:它回不来了,挂着的问题要被
/// 撤掉、回合要被取消,否则那一轮永远 running、下一轮看不见它(09-09 失忆根因)。
#[test]
fn dropped_one_shot_ipc_turn_cancels_the_run_and_its_question() {
    let (manager, cancel_rx) = manager_with_run("run_test");
    let broker = QuestionBroker::new();
    let (responder, mut response) = oneshot::channel();
    let question_id = broker.insert("run_test", sample_question(), responder);
    drop(IpcRunGuard {
        manager,
        run_id: "run_test".to_string(),
        finished: false,
        one_shot: true,
        questions: Some(broker.clone()),
    });
    assert!(
        *cancel_rx.borrow(),
        "one-shot disconnect must cancel the run"
    );
    assert!(matches!(
        response.try_recv().unwrap(),
        QuestionResponse::Cancelled
    ));
    assert!(!broker.pending.lock().unwrap().contains_key(&question_id));
}

/// 正常收尾(finished)的一次性回合断线不取消:回合已经跑完了。
#[test]
fn finished_one_shot_ipc_turn_does_not_cancel() {
    let (manager, cancel_rx) = manager_with_run("run_test");
    drop(IpcRunGuard {
        manager,
        run_id: "run_test".to_string(),
        finished: true,
        one_shot: true,
        questions: None,
    });
    assert!(!*cancel_rx.borrow());
}

#[test]
fn closing_question_resumes_run_without_answers() {
    let broker = QuestionBroker::new();
    let (responder, mut response) = oneshot::channel();
    let question_id = broker.insert("run_test", sample_question(), responder);
    let mut resumed_run = None;

    broker
        .close(&question_id, |run_id| {
            assert!(response.try_recv().is_err());
            resumed_run = Some(run_id.to_string())
        })
        .unwrap();

    assert_eq!(resumed_run.as_deref(), Some("run_test"));
    assert!(matches!(
        response.try_recv().unwrap(),
        QuestionResponse::Closed
    ));
    assert!(!broker.pending.lock().unwrap().contains_key(&question_id));
}
