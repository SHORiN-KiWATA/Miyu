//! 排队消息与流水落盘。

use super::shared::*;
use crate::agent::*;
use crate::config::AppConfig;
use crate::tools::{empty_parameters, ToolSpec};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn queue_ingress_waits_for_a_reserved_tool_followup() {
    let barrier = Arc::new(QueueIngressBarrier::default());
    barrier.tool_started("call_1");
    let reservation = barrier
        .try_reserve()
        .expect("active tool accepts follow-up");
    barrier.tool_finished("call_1");

    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        barrier.wait_for_reserved_ingress()
    )
    .await
    .is_err());
    assert!(barrier.try_reserve().is_none());

    drop(reservation);
    tokio::time::timeout(
        Duration::from_millis(100),
        barrier.wait_for_reserved_ingress(),
    )
    .await
    .expect("released follow-up reservation wakes the agent");
}

#[test]
fn queue_ingress_tracks_parallel_tool_calls_by_id() {
    let barrier = Arc::new(QueueIngressBarrier::default());
    barrier.tool_started("call_1");
    barrier.tool_started("call_2");
    barrier.tool_finished("call_1");
    assert!(barrier.try_reserve().is_some());
    barrier.tool_finished("call_2");
    assert!(barrier.try_reserve().is_none());
}

#[test]
fn journal_persists_a_stream_batch_before_displaying_it() {
    let temp = tempfile::tempdir().unwrap();
    let state = crate::state::StateStore::new(&test_paths(temp.path())).unwrap();
    state
        .start_turn("journal-turn", "long task", std::process::id())
        .unwrap();
    let mut sink = TurnJournalSink::new(state.clone(), "journal-turn".to_string(), 0);
    let mut displayed = Vec::new();
    {
        let mut on_event = |event| {
            if let AgentEvent::Chunk(chunk) = event {
                displayed.push(chunk.text);
            }
            Ok(())
        };
        sink.emit(
            AgentEvent::Chunk(ChatStreamChunk {
                kind: ChatStreamKind::Content,
                text: "durable partial".to_string(),
            }),
            &mut on_event,
        )
        .unwrap();
    }
    assert!(displayed.is_empty());
    assert!(state.load_turns().unwrap()[0].journal_events.is_empty());

    {
        let mut on_event = |event| {
            if let AgentEvent::Chunk(chunk) = event {
                displayed.push(chunk.text);
            }
            Ok(())
        };
        sink.emit(AgentEvent::SpinnerTick, &mut on_event).unwrap();
    }
    assert_eq!(displayed, ["durable partial"]);
    assert_eq!(state.load_turns().unwrap()[0].journal_events.len(), 1);

    state.interrupt_turn("journal-turn").unwrap();
    assert!(state.load_turns().unwrap()[0]
        .assistant_content
        .contains("durable partial"));
}

#[test]
fn journal_flush_precedes_queued_prompt_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let state = crate::state::StateStore::new(&test_paths(temp.path())).unwrap();
    state
        .start_turn("boundary-turn", "long task", std::process::id())
        .unwrap();
    state
        .enqueue_prompt("q1", "followup", "followup", &[])
        .unwrap();
    let mut sink = TurnJournalSink::new(state.clone(), "boundary-turn".to_string(), 0);
    let mut displayed = Vec::new();
    let mut transport = |event| {
        if let AgentEvent::Chunk(chunk) = event {
            displayed.push(chunk.text);
        }
        Ok(())
    };
    let mut journaled = |event| sink.emit(event, &mut transport);

    journaled(AgentEvent::Chunk(ChatStreamChunk {
        kind: ChatStreamKind::Content,
        text: "answer before followup".to_string(),
    }))
    .unwrap();
    journaled(AgentEvent::FlushJournal).unwrap();
    state
        .consume_queued_prompts(
            "boundary-turn",
            &[("q1".to_string(), "followup".to_string())],
            Some("answer before followup"),
            None,
        )
        .unwrap();
    journaled(AgentEvent::QueuedPromptsConsumed {
        prompt_ids: vec!["q1".to_string()],
        mode: AgentMode::Normal,
        provider_id: None,
        model: None,
    })
    .unwrap();

    let events = state.load_turns().unwrap()[0].journal_events.clone();
    assert_eq!(displayed, ["answer before followup"]);
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        ["assistant_content", "queued_prompts_consumed"]
    );
}

#[test]
fn interrupted_redo_replays_prefix_followups_before_new_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let followup = |prompt_id: &str, content: &str, preceding: &str| crate::state::TurnFollowup {
        prompt_id: prompt_id.to_string(),
        content: content.to_string(),
        display_content: content.to_string(),
        attachments: Vec::new(),
        uploaded_attachments: Vec::new(),
        submitted_at: String::new(),
        preceding_assistant_content: Some(preceding.to_string()),
        preceding_assistant_reasoning: None,
        preceding_assistant_provider_id: None,
        preceding_assistant_model: None,
        context_messages_json: "[]".to_string(),
    };
    let mut turn = crate::state::Turn {
        turn_id: "redo-turn".to_string(),
        seq: 1,
        user_content: "initial".to_string(),
        display_content: "initial".to_string(),
        user_timestamp: String::new(),
        assistant_content: crate::state::pending_placeholder().to_string(),
        assistant_reasoning: None,
        assistant_provider_id: None,
        assistant_model: None,
        assistant_timestamp: None,
        status: crate::state::TurnStatus::Interrupted,
        tool_reports: Vec::new(),
        tool_flow: Vec::new(),
        question_exchanges: vec![
            QuestionExchange {
                questions: vec![crate::question::QuestionPrompt {
                    header: "Route".to_string(),
                    question: "Pick a route".to_string(),
                    options: vec![crate::question::QuestionOption {
                        label: "A".to_string(),
                        description: "".to_string(),
                    }],
                    multiple: false,
                    custom: false,
                }],
                answers: vec![vec!["A".to_string()]],
                answered_at: String::new(),
            },
            QuestionExchange {
                questions: vec![crate::question::QuestionPrompt {
                    header: "Branch".to_string(),
                    question: "Current branch question".to_string(),
                    options: vec![crate::question::QuestionOption {
                        label: "B".to_string(),
                        description: "".to_string(),
                    }],
                    multiple: false,
                    custom: false,
                }],
                answers: vec![vec!["B".to_string()]],
                answered_at: String::new(),
            },
        ],
        followups: vec![
            followup("q1", "edited first followup", "first answer"),
            followup("q2", "new followup", "after q1"),
        ],
        attachments: Vec::new(),
        hidden: false,
        is_summary: false,
        owner_pid: None,
        token_total: 0,
        token_prompt: 0,
        token_cache_read: 0,
        token_usage_estimated: false,
        revision: 1,
        journal_events: vec![
            crate::state::TurnJournalEvent {
                event_id: 0,
                revision: 1,
                segment_index: 0,
                kind: "redo_prefix_question_count".to_string(),
                call_id: None,
                name: None,
                text_payload: Some("1".to_string()),
                blob_payload: None,
                ok: None,
            },
            crate::state::TurnJournalEvent {
                event_id: 1,
                revision: 1,
                segment_index: 0,
                kind: "assistant_content".to_string(),
                call_id: None,
                name: None,
                text_payload: Some("after q1".to_string()),
                blob_payload: None,
                ok: None,
            },
            crate::state::TurnJournalEvent {
                event_id: 2,
                revision: 1,
                segment_index: 0,
                kind: "queued_prompts_consumed".to_string(),
                call_id: None,
                name: None,
                text_payload: Some("[\"q2\"]".to_string()),
                blob_payload: None,
                ok: None,
            },
            crate::state::TurnJournalEvent {
                event_id: 3,
                revision: 1,
                segment_index: 1,
                kind: "assistant_content".to_string(),
                call_id: None,
                name: None,
                text_payload: Some("after q2".to_string()),
                blob_payload: None,
                ok: None,
            },
        ],
        context_messages: Vec::new(),
    };

    let messages = interrupted_turn_replay_messages(&agent, &turn);
    let text_messages = messages
        .iter()
        .filter_map(|message| match message.content.as_ref() {
            Some(ChatContent::Text(text)) => Some((message.role.as_str(), text.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let q1 = text_messages
        .iter()
        .position(|(_, text)| *text == "edited first followup")
        .unwrap();
    let clarification = text_messages
        .iter()
        .position(|(_, text)| text.contains("Pick a route"))
        .unwrap();
    assert!(!text_messages
        .iter()
        .any(|(_, text)| text.contains("Current branch question")));
    let after_q1 = text_messages
        .iter()
        .position(|(_, text)| *text == "after q1")
        .unwrap();
    let q2 = text_messages
        .iter()
        .position(|(_, text)| *text == "new followup")
        .unwrap();
    let after_q2 = text_messages
        .iter()
        .position(|(_, text)| *text == "after q2")
        .unwrap();
    assert!(clarification < q1);
    assert!(q1 < after_q1);
    assert!(after_q1 < q2);
    assert!(q2 < after_q2);

    turn.journal_events
        .retain(|event| event.kind != "redo_prefix_question_count");
    turn.journal_events.push(crate::state::TurnJournalEvent {
        event_id: 4,
        revision: 1,
        segment_index: 1,
        kind: "tool_result".to_string(),
        call_id: Some("question-call".to_string()),
        name: Some("ask_question".to_string()),
        text_payload: Some("{\"status\":\"answered\"}".to_string()),
        blob_payload: None,
        ok: Some(true),
    });
    let legacy_messages = interrupted_turn_replay_messages(&agent, &turn);
    let legacy_text = legacy_messages
        .iter()
        .filter_map(|message| match message.content.as_ref() {
            Some(ChatContent::Text(text)) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(legacy_text.iter().any(|text| text.contains("Pick a route")));
    assert!(!legacy_text
        .iter()
        .any(|text| text.contains("Current branch question")));
}

#[tokio::test]
async fn queued_prompt_continues_after_a_completed_model_call() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    config.providers[0].model_modalities.insert(
        "test-model".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );
    let control =
        AgentTurnControl::new(AgentMode::Normal, ToolRegistry::new(), ToolRegistry::new());
    let server_control = control.clone();
    let (request_tx, request_rx) = oneshot::channel();
    let (redo_request_tx, redo_request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        server_control.set_mode(AgentMode::Dev);
        write_test_sse(
            &mut first,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"first reasoning\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"first answer\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut second).await;
        let _ = request_tx.send(request);
        write_test_sse(
            &mut second,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"continued answer\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;

        let (mut third, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut third).await;
        let _ = redo_request_tx.send(request);
        write_test_sse(
            &mut third,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"redone answer\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    state
        .enqueue_prompt(
            "q1",
            "queued followup",
            "queued followup",
            &[QueuedPromptAttachment::Binary {
                mime: "image/png".to_string(),
                data_base64: base64::engine::general_purpose::STANDARD.encode(b"image-data"),
            }],
        )
        .unwrap();

    let result = agent
        .chat_stream_with_control("initial prompt", &[], &control, |_| Ok(()))
        .await
        .unwrap();

    assert_eq!(result.content, "continued answer");
    assert_eq!(agent.mode(), AgentMode::Dev);
    let request: serde_json::Value = serde_json::from_slice(&request_rx.await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    let first_answer = messages
        .iter()
        .position(|message| message["role"] == "assistant" && message["content"] == "first answer")
        .unwrap();
    let followup = messages
        .iter()
        .position(|message| {
            message["role"] == "user"
                && message["content"].as_array().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|part| part["type"] == "text" && part["text"] == "queued followup")
                        && parts.iter().any(|part| part["type"] == "image_url")
                })
        })
        .unwrap();
    // 跨轮思考回放退役:live 与回放同刀,followup 边界不再夹带思维链。
    assert!(!messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|content| content.contains("<previous_assistant_reasoning>"))
    }));
    assert!(first_answer < followup);
    let turns = state.load_turns().unwrap();
    assert_eq!(
        turns[0].followups[0].preceding_assistant_content.as_deref(),
        Some("first answer")
    );
    assert_eq!(
        turns[0].followups[0]
            .preceding_assistant_reasoning
            .as_deref(),
        Some("first reasoning")
    );
    // followup 随发的尾巴(图片路径提示)与它一起化石化,回放紧跟在正文后。
    let fossil_tail = turns[0].followups[0].context_messages();
    assert!(fossil_tail.iter().any(|message| matches!(
        message.content.as_ref(),
        Some(ChatContent::Text(text)) if text.contains("已保存到临时文件")
    )));
    // 活体第二次请求里同一份尾巴紧跟 followup 之后,且 runtime 不再原地改写。
    assert!(messages[followup + 1]["content"]
        .as_str()
        .is_some_and(
            |content| content.starts_with("<runtime now=") || content.contains("已保存到临时文件")
        ));
    let history = agent.chat_messages("", "next prompt").unwrap().0;
    let replayed_followup = history
        .iter()
        .position(|message| {
            matches!(
                message.content.as_ref(),
                Some(ChatContent::Parts(parts))
                    if parts.iter().any(|part| matches!(part, ChatContentPart::ImageUrl { .. }))
            )
        })
        .unwrap();
    assert!(matches!(
        history[replayed_followup + 1].content.as_ref(),
        Some(ChatContent::Text(text)) if text.starts_with("<runtime now=") || text.contains("已保存到临时文件")
    ));
    let candidate = state.redo_candidate().unwrap().unwrap();
    let redo = agent
        .redo_stream_with_control(
            &candidate,
            vec![RedoPromptInput {
                prompt_id: "q1".to_string(),
                content: "edited followup".to_string(),
                display_content: "edited followup".to_string(),
                images: vec![Some(PastedImage::Binary(ClipboardImage::new(
                    "image/png".to_string(),
                    b"image-data".to_vec(),
                )))],
            }],
            &control,
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(redo.content, "redone answer");
    let redo_request: serde_json::Value =
        serde_json::from_slice(&redo_request_rx.await.unwrap()).unwrap();
    let redo_messages = redo_request["messages"].as_array().unwrap();
    assert!(redo_messages
        .iter()
        .any(|message| { message["role"] == "assistant" && message["content"] == "first answer" }));
    assert!(redo_messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["type"] == "text" && part["text"] == "edited followup")
            })
    }));
    assert!(!redo_messages.iter().any(|message| {
        message["role"] == "assistant" && message["content"] == "continued answer"
    }));
    let turn = state.load_turns().unwrap().remove(0);
    assert_eq!(turn.assistant_content, "redone answer");
    assert_eq!(turn.followups[0].content, "edited followup");
    assert_eq!(turn.revision, 1);
    server.await.unwrap();
}

#[tokio::test]
async fn supersede_restarts_the_same_turn_without_replaying_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    let (partial_tx, partial_rx) = oneshot::channel();
    let (second_request_tx, second_request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        first
            .write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "content-type: text/event-stream\r\n",
                    "connection: close\r\n\r\n",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"discarded partial\"}}]}\n\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        first.flush().await.unwrap();
        let _ = partial_tx.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(first);

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut second).await;
        let _ = second_request_tx.send(request);
        write_test_sse(
            &mut second,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"updated final\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let signal = Arc::new(TurnSupersedeSignal::default());
    let mut control =
        AgentTurnControl::new(AgentMode::Normal, ToolRegistry::new(), ToolRegistry::new());
    control.set_supersede_signal(signal.clone());
    let events = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let event_log = events.clone();
    let chat = agent.chat_stream_with_control("original", &[], &control, move |event| {
        if matches!(event, AgentEvent::GenerationSuperseded { .. }) {
            event_log.lock().unwrap().push("superseded");
        }
        Ok(())
    });
    let enqueue = async {
        partial_rx.await.unwrap();
        state
            .enqueue_prompt("update", "changed requirement", "changed requirement", &[])
            .unwrap();
        signal.trigger();
    };
    let (result, ()) = tokio::join!(chat, enqueue);
    let result = result.unwrap();
    assert_eq!(result.content, "updated final");
    assert_eq!(&*events.lock().unwrap(), &["superseded"]);
    let request: Value = serde_json::from_slice(&second_request_rx.await.unwrap()).unwrap();
    let serialized = serde_json::to_string(&request["messages"]).unwrap();
    assert!(serialized.contains("changed requirement"));
    assert!(!serialized.contains("discarded partial"));
    let turns = state.load_turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].assistant_content, "updated final");
    assert_eq!(turns[0].followups.len(), 1);
    assert!(turns[0].followups[0].preceding_assistant_content.is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn queued_prompts_are_consumed_after_tools_with_dispatch_time_mode() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;
    config.skills.enabled = false;
    config.memory.enabled = false;

    let mut normal_tools = ToolRegistry::new();
    normal_tools.register(ToolSpec::new(
        "queue_boundary_tool",
        "returns a fixed result",
        empty_parameters(),
        |_| async { Ok("tool finished".to_string()) },
    ));
    let control =
        AgentTurnControl::new(AgentMode::Normal, normal_tools.clone(), ToolRegistry::new());
    let server_control = control.clone();
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        server_control.set_mode(AgentMode::Dev);
        write_test_sse(
            &mut first,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"queue_boundary_tool\",\"arguments\":\"{}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut second).await;
        let _ = request_tx.send(request);
        write_test_sse(
            &mut second,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"final answer\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        normal_tools,
        AgentMode::Normal,
    )
    .unwrap();
    state
        .enqueue_prompt("q1", "first followup", "first followup", &[])
        .unwrap();
    state
        .enqueue_prompt("q2", "second followup", "second followup", &[])
        .unwrap();
    let mut consumed = None;

    let result = agent
        .chat_stream_with_control("initial prompt", &[], &control, |event| {
            if let AgentEvent::QueuedPromptsConsumed {
                prompt_ids, mode, ..
            } = event
            {
                consumed = Some((prompt_ids, mode));
            }
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(result.content, "final answer");
    assert_eq!(agent.mode(), AgentMode::Dev);
    assert_eq!(
        consumed,
        Some((vec!["q1".to_string(), "q2".to_string()], AgentMode::Dev))
    );
    let request: serde_json::Value = serde_json::from_slice(&request_rx.await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages
        .iter()
        .any(|message| { message["role"] == "user" && message["content"] == "first followup" }));
    assert!(messages
        .iter()
        .any(|message| { message["role"] == "user" && message["content"] == "second followup" }));
    assert!(messages
        .iter()
        .any(|message| { message["role"] == "tool" && message["content"] == "tool finished" }));
    assert!(state.load_queued_prompts().unwrap().is_empty());
    let turns = state.load_turns().unwrap();
    assert_eq!(turns[0].followups.len(), 2);
    assert_eq!(turns[0].assistant_content, "final answer");
    server.await.unwrap();
}

/// stub 模式下 ask_question 必须直接发完整契约，而不是一句摘要 + 空壳。
///
/// 它是唯一绕过 `ToolRegistry` 分发的工具（`turn_loop` 按名字特判要交互通道），
/// 所以注册表那道 `coerce_declared_shapes` 到不了它——**模型看到什么形状，就
/// 照着填什么形状**，没有第二道兜底。`always_loaded: false` 时它在 stub 里只
/// 剩 60 字摘要和 `{"type":"object"}`，模型拿不到 `questions` 是数组、
/// `options` 的元素是对象这些信息，于是把 questions 填成字符串数组、options
/// 填成字符串——下面那个用例里的补救解析就是这么被逼出来的。
///
/// 代价是每个交互会话的工具目录多约 2.5 KiB（≈650 token）常驻缓存前缀，按
/// 十分之一计价，可以忽略。
#[test]
fn ask_question_ships_its_full_schema_in_stub_mode() {
    let mut tools = ToolRegistry::new();
    crate::tools::register_ask_question(&mut tools);
    let definition = tools
        .stub_definitions()
        .into_iter()
        .find(|definition| definition.function.name == "ask_question")
        .expect("ask_question 应当在注册表里");
    let questions = &definition.function.parameters["properties"]["questions"];
    assert_eq!(
        questions["type"], "array",
        "stub 里拿不到 questions 的数组契约，模型只能猜形状"
    );
    let fields = &questions["items"]["properties"];
    for field in ["header", "question", "options"] {
        assert!(!fields[field].is_null(), "缺少 {field} 的字段契约");
    }
    assert_eq!(
        fields["options"]["items"]["type"], "object",
        "options 的元素必须声明成对象，否则模型会填字符串数组"
    );
}

/// 端到端走一遍 ask_question 的分发：模型把 `questions` 序列化成字符串发过来，
/// 回合应当照常弹出提问、拿到回答、继续跑完，而不是回一条 tool error。
///
/// 这个工具不走 `ToolRegistry` 分发（要拿交互通道），所以注册表那道
/// `coerce_declared_shapes` 到不了它——只有从这条路真跑一遍才算验过。
/// 上面那条契约用例修好之后这种形状应当变罕见，但**兼容解析不能撤**：
/// 模型仍然会偶尔填错，而这里没有第二道兜底。
#[tokio::test]
async fn ask_question_accepts_questions_serialized_as_a_json_string() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;

    // 手写多层转义太容易写坏，让 serde 生成:questions 是一个「内容为 JSON 数组
    // 的字符串」，正是实测里模型发来的形状。
    let questions_as_string = serde_json::to_string(&serde_json::json!([{
        "header": "选项",
        "question": "选哪个？",
        "options": [{"label": "A", "description": "甲"}],
    }]))
    .unwrap();
    let arguments = serde_json::to_string(&serde_json::json!({
        "questions": questions_as_string,
    }))
    .unwrap();
    let first_chunk = serde_json::to_string(&serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call-1",
            "type": "function",
            "function": {"name": "ask_question", "arguments": arguments},
        }]}}],
    }))
    .unwrap();

    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        write_test_sse(
            &mut first,
            &format!(
                concat!(
                    "data: {}\n\n",
                    "data: {{\"choices\":[{{\"finish_reason\":\"tool_calls\",\"delta\":{{}}}}]}}\n\n",
                    "data: [DONE]\n\n"
                ),
                first_chunk
            ),
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut second).await;
        write_test_sse(
            &mut second,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"收到\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
        request
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut tools = ToolRegistry::new();
    crate::tools::register_ask_question(&mut tools);
    let mut agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        tools,
        AgentMode::Normal,
    )
    .unwrap();

    let mut asked = Vec::new();
    let mut seen = Vec::new();
    let result = agent
        .chat_stream("问我个问题", |event| {
            match &event {
                AgentEvent::ToolResult {
                    name, ok, output, ..
                } => seen.push(format!("ToolResult {name} ok={ok} {output}")),
                AgentEvent::AskQuestion { .. } => seen.push("AskQuestion".to_string()),
                _ => {}
            }
            if let AgentEvent::AskQuestion {
                request, responder, ..
            } = event
            {
                asked.push(request.clone());
                let _ = responder.send(QuestionResponse::Answered(vec![vec!["A".to_string()]]));
            }
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(asked.len(), 1, "应当真的弹出一次提问，实际事件：{seen:?}");
    assert_eq!(asked[0].questions.len(), 1);
    assert_eq!(asked[0].questions[0].header, "选项");
    assert_eq!(asked[0].questions[0].options[0].label, "A");
    assert_eq!(result.content, "收到");

    // 回给模型的 tool 消息应当是回答，不是报错
    let request: serde_json::Value = serde_json::from_slice(&server.await.unwrap()).unwrap();
    let tool_reply = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .unwrap()
        .clone();
    let reply_text = tool_reply["content"].as_str().unwrap_or_default();
    assert!(
        !reply_text.contains("tool error"),
        "不该回 tool error，实际：{reply_text}"
    );
}
