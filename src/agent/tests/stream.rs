//! 流式回合：排队、打断、并发与超越。

use super::shared::*;
use crate::agent::*;
use crate::config::AppConfig;
use crate::tools::{empty_parameters, ToolSpec};
use tokio::net::TcpListener;

#[test]
fn tool_call_stream_announces_preparation_for_slow_argument_tools() {
    let mut filter = ReasoningTitleFilter::default();
    let mut prepared = Vec::new();
    let mut streamed = Vec::new();
    let mut on_event = |event| {
        match event {
            AgentEvent::ToolPreparing { name, .. } => prepared.push(name),
            AgentEvent::Chunk(chunk) if chunk.kind == ChatStreamKind::ToolCall => {
                streamed.push(chunk.text)
            }
            _ => {}
        }
        Ok(())
    };
    let names = [
        "apply_patch",
        "apply_artifact_patch",
        "write_file",
        "edit_string",
        "run_command",
        "task",
        "ask_question",
        // Arguments arrive in one chunk: a hint here would only flicker.
        "read_file",
    ];
    for name in names {
        emit_filtered_chunk(
            ChatStreamChunk {
                kind: ChatStreamKind::ToolCall,
                text: name.to_string(),
            },
            &mut filter,
            &mut on_event,
        )
        .unwrap();
    }
    assert_eq!(
        prepared,
        [
            "apply_patch",
            "apply_artifact_patch",
            "write_file",
            "edit_string",
            "run_command",
            "task",
            "ask_question"
        ]
    );
    assert_eq!(streamed, names);
}

/// 上一个用例每次调用都新起一个计数器，测的是「单个工具够不够慢」。
/// 这里共用一个计数器，模拟同一条 assistant 消息里连着来的多个调用。
#[test]
fn tool_call_stream_announces_preparation_for_later_calls_in_a_batch() {
    let mut filter = ReasoningTitleFilter::default();
    let mut seen = 0usize;
    let mut prepared = Vec::new();
    let mut on_event = |event| {
        if let AgentEvent::ToolPreparing { name, batch } = event {
            prepared.push((name, batch));
        }
        Ok(())
    };
    for name in ["read_file", "read_file", "glob"] {
        emit_filtered_chunk_at(
            ChatStreamChunk {
                kind: ChatStreamKind::ToolCall,
                text: name.to_string(),
            },
            Instant::now(),
            &mut filter,
            &mut seen,
            &mut on_event,
        )
        .unwrap();
    }
    // 第一个调用照旧不提示——单看 read_file 的参数一个 chunk 就到了,
    // 提示只会闪一下。后面两个才知道这是批量。
    assert_eq!(
        prepared,
        [("read_file".to_string(), true), ("glob".to_string(), true)]
    );
}

#[test]
fn structured_tool_business_failure_marks_the_event_failed() {
    assert!(!tool_output_succeeded(r#"{"success":false}"#));
    assert!(!tool_output_succeeded(r#"{"ok":false}"#));
    assert!(tool_output_succeeded(r#"{"success":true}"#));
    assert!(tool_output_succeeded("plain tool output"));
}

#[tokio::test]
async fn parallel_task_calls_run_concurrently_and_map_outputs() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(crate::tools::ToolSpec::new(
        "task",
        "stub subagent",
        crate::tools::empty_parameters(),
        |args| async move {
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(format!(
                "done:{}",
                args.get("n")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            ))
        },
    ));
    let agent = Agent::new(
        config,
        &paths,
        state.clone(),
        client,
        registry,
        AgentMode::Normal,
    )
    .unwrap();

    let calls: Vec<crate::llm::ToolCall> = (0..3)
        .map(|index| crate::llm::ToolCall {
            id: format!("call_{index}"),
            kind: "function".to_string(),
            function: crate::llm::ToolCallFunction {
                name: "task".to_string(),
                arguments: format!(r#"{{"n":"{index}"}}"#),
            },
        })
        .collect();
    let mut events = Vec::new();
    let started = std::time::Instant::now();
    let outputs = agent
        .execute_parallel_task_calls(&calls, &mut |event| {
            match &event {
                AgentEvent::ToolCall { call_id, .. } => events.push((call_id.clone(), "call")),
                AgentEvent::ToolResult {
                    call_id, ok: true, ..
                } => events.push((call_id.clone(), "ok")),
                AgentEvent::ToolResult {
                    call_id, ok: false, ..
                } => events.push((call_id.clone(), "err")),
                _ => {}
            }
            Ok(())
        })
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(outputs.len(), 3);
    for index in 0..3 {
        assert_eq!(outputs[&index].output, format!("done:{index}"));
    }
    // Three 80ms tasks run concurrently, not sequentially (~240ms).
    assert!(
        elapsed < Duration::from_millis(200),
        "tasks did not run in parallel: {elapsed:?}"
    );
    for index in 0..3 {
        let call_id = format!("call_{index}");
        assert!(events.contains(&(call_id.clone(), "call")));
        assert!(events.contains(&(call_id, "ok")));
    }

    // Fewer than two task calls: empty map, serial path handles it.
    let single = agent
        .execute_parallel_task_calls(&calls[..1], &mut |_| Ok(()))
        .await
        .unwrap();
    assert!(single.is_empty());
}

#[tokio::test]
async fn responses_tool_round_uses_previous_response_id_and_only_new_input() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;
    config.tools.loading_mode = "full".to_string();
    config.skills.enabled = false;
    config.memory.enabled = false;
    config.providers[0].protocol = "openai-responses".to_string();
    config.providers[0].models = vec!["gpt-5".to_string()];
    config.providers[0].default_model = "gpt-5".to_string();

    let mut tools = ToolRegistry::new();
    tools.register(ToolSpec::new(
        "responses_continuation_tool",
        "returns a fixed result",
        empty_parameters(),
        |_| async { Ok("tool finished".to_string()) },
    ));
    let control = AgentTurnControl::new(AgentMode::Normal, tools.clone(), tools.clone());
    let server_control = control.clone();

    let (first_request_tx, first_request_rx) = oneshot::channel();
    let (second_request_tx, second_request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let first_request = read_test_http_request(&mut first).await;
        let _ = first_request_tx.send(first_request);
        server_control.set_mode(AgentMode::Dev);
        write_test_sse(
            &mut first,
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"responses_continuation_tool\",\"arguments\":\"\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{}\"}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"responses_continuation_tool\",\"arguments\":\"{}\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2,\"total_tokens\":7}}}\n\n"
            ),
        )
        .await;

        let (mut second, _) = listener.accept().await.unwrap();
        let second_request = read_test_http_request(&mut second).await;
        let _ = second_request_tx.send(second_request);
        write_test_sse(
            &mut second,
            concat!(
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\n",
                "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_2\",\"delta\":\"final answer\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\"}}\n\n"
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
        tools,
        AgentMode::Normal,
    )
    .unwrap();
    state
        .enqueue_prompt("q1", "queued followup", "queued followup", &[])
        .unwrap();

    let result = agent
        .chat_stream_with_control("initial prompt", &[], &control, |_| Ok(()))
        .await
        .unwrap();

    assert_eq!(result.content, "final answer");
    assert_eq!(agent.mode(), AgentMode::Dev);
    assert!(result.responses_continuation.is_none());
    assert!(result.usage_estimated);
    let tool_only_tokens =
        overflow::estimate_messages_tokens(&[ChatMessage::tool("call_1", "tool finished")]) as u64;
    assert!(result.usage.as_ref().unwrap().prompt_tokens > 5 + tool_only_tokens);
    let first_request: Value = serde_json::from_slice(&first_request_rx.await.unwrap()).unwrap();
    assert!(first_request.get("previous_response_id").is_none());
    assert!(first_request["input"].as_array().is_some_and(|input| {
        input.iter().any(|item| item["role"] == "user")
            && input.iter().any(|item| item["role"] == "system")
    }));

    let second_request: Value = serde_json::from_slice(&second_request_rx.await.unwrap()).unwrap();
    assert_eq!(second_request["previous_response_id"], "resp_1");
    let input = second_request["input"].as_array().unwrap();
    let function_output = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(function_output["call_id"], "call_1");
    assert_eq!(function_output["output"], "tool finished");
    let function_index = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .unwrap();
    // Responses-style user items carry their text as `input_text` parts,
    // so the block has to be read through both shapes.
    let item_text = |item: &Value| -> String {
        match &item["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    };
    let is_mode_update = |item: &Value| {
        let text = item_text(item);
        item["role"] == "user" && text.contains("<mode-update active=\"dev\">")
    };
    let mode_index = input.iter().position(is_mode_update).unwrap();
    assert!(input.iter().any(is_mode_update));
    let queued_index = input
        .iter()
        .position(|item| {
            item["role"] == "user"
                && item["content"].as_array().is_some_and(|parts| {
                    parts.iter().any(|part| {
                        part["type"] == "input_text" && part["text"] == "queued followup"
                    })
                })
        })
        .unwrap();
    assert!(input.iter().any(|item| {
        item["role"] == "user"
            && item["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["type"] == "input_text" && part["text"] == "queued followup")
            })
    }));
    assert!(function_index < mode_index && mode_index < queued_index);
    assert!(!serde_json::to_string(input)
        .unwrap()
        .contains("initial prompt"));
    assert!(second_request["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["name"] == "responses_continuation_tool")
    }));
    assert_eq!(
        state.load_turns().unwrap()[0].assistant_content,
        "final answer"
    );
    server.await.unwrap();
}

/// guard 拒绝是软失败:命令拒绝子串拦下 run_command,回给模型一条
/// tool error 让它换路,轮次存活拿到最终回答——而不是炸掉整轮。
#[tokio::test]
async fn guard_denied_tool_soft_fails_and_turn_continues() {
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
        "run_command",
        "runs commands",
        empty_parameters(),
        |_| async { Ok("should never run".to_string()) },
    ));
    normal_tools.add_guard(crate::tools::command_deny_guard(vec![
        "rm -rf /".to_string()
    ]));
    let control = AgentTurnControl::new(
        AgentMode::Normal,
        normal_tools.clone(),
        normal_tools.clone(),
    );
    let (request_tx, request_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        write_test_sse(
            &mut first,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"sudo rm -rf /\\\"}\"}}]}}]}\n\n",
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
                "data: {\"choices\":[{\"delta\":{\"content\":\"recovered answer\"}}]}\n\n",
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

    let result = agent
        .chat_stream_with_control("initial prompt", &[], &control, |_| Ok(()))
        .await
        .unwrap();

    assert_eq!(result.content, "recovered answer");
    let request: serde_json::Value = serde_json::from_slice(&request_rx.await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["role"] == "tool"
            && message["content"].as_str().is_some_and(|content| {
                content.contains("denied pattern") || content.contains("被禁止的模式")
            })
    }));
    server.await.unwrap();
}

/// 回合中途死掉时，「已经调过哪些工具、拿到什么结果」必须已经落盘。
///
/// `tool_flow` 以前只在整个回合跑完之后写一次（`stream.rs` 的
/// `set_turn_tool_flow`）。可正文和工具报告都能从流水物化出来，唯独它不能——
/// 而它正是 `history.rs` 回放给模型的那一份。丢了它，模型下一轮只看到半截
/// 文字，会把已经跑过的命令、读过的文件原样再来一遍。这就是「崩溃后续不上」。
///
/// 这里用「第一轮返回工具调用、第二轮请求直接断连」模拟中途死亡：回合以失败
/// 告终，但那次工具调用的记录必须留在库里。
#[tokio::test]
async fn a_turn_that_dies_mid_loop_keeps_the_tools_it_already_ran() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;

    let mut tools = ToolRegistry::new();
    tools.register(ToolSpec::new(
        "run_command",
        "runs commands",
        empty_parameters(),
        |_| async { Ok("已经跑过了，别再跑一遍".to_string()) },
    ));
    let control = AgentTurnControl::new(AgentMode::Normal, tools.clone(), tools.clone());

    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut first).await;
        write_test_sse(
            &mut first,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
        // 工具轮之后的这次请求直接断连 —— 相当于进程在这里死掉。
        let (second, _) = listener.accept().await.unwrap();
        drop(second);
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
        tools,
        AgentMode::Normal,
    )
    .unwrap();

    let outcome = agent
        .chat_stream_with_control("跑一下 ls", &[], &control, |_| Ok(()))
        .await;
    assert!(outcome.is_err(), "第二轮断连，回合应当失败");

    let turn = state.load_turns().unwrap().pop().expect("回合应当已落库");
    assert!(
        !turn.tool_flow.is_empty(),
        "回合中途死掉，tool_flow 却是空的——模型下一轮会把 run_command 再跑一遍"
    );
    let ran = turn
        .tool_flow
        .iter()
        .flat_map(|round| round.calls.iter())
        .any(|call| call.name == "run_command");
    assert!(ran, "tool_flow 里没有已经执行过的 run_command");
    server.await.unwrap();
}

/// 同参复读闸(08-23 取证,08-24 二版:全程无提示文本):第 3 个相同轮起
/// 不再真执行、回灌上一轮真实结果字节;第 6 个相同轮烧保险丝——此后请
/// 求不再带工具;模型(桩)仍坚持发调用时静默收束,任何受众正文都不掺
/// 机器文本。退回闸前这个用例会陪桩服务器转满 100 轮后在断言处报红。
#[tokio::test]
async fn identical_tool_call_loop_is_skipped_then_fused() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;

    use std::sync::atomic::{AtomicUsize, Ordering};
    let executions = Arc::new(AtomicUsize::new(0));
    let executions_probe = executions.clone();
    let mut tools = ToolRegistry::new();
    tools.register(ToolSpec::new(
        "web_search",
        "searches",
        empty_parameters(),
        move |_| {
            let executions = executions.clone();
            async move {
                executions.fetch_add(1, Ordering::SeqCst);
                Ok("{\"ok\": true, \"results\": \"same results\"}".to_string())
            }
        },
    ));
    let control = AgentTurnControl::new(AgentMode::Normal, tools.clone(), tools.clone());

    let server = tokio::spawn(async move {
        // 桩模型:每次请求都发一模一样的 web_search 调用,最多陪 100 轮。
        // 保险丝在第 6 个相同轮收束,轮数远到不了 100——到了就是闸没工作。
        for _ in 0..100 {
            let Ok((mut round, _)) = listener.accept().await else {
                return;
            };
            let _ = read_test_http_request(&mut round).await;
            write_test_sse(
                &mut round,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"same\\\"}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        }
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
        tools,
        AgentMode::Normal,
    )
    .unwrap();

    let reply = agent
        .chat_stream_with_control("搜一下", &[], &control, |_| Ok(()))
        .await
        .unwrap();
    // 真执行只有前两轮(首轮+第 2 个相同轮),后面全被闸掉。
    assert_eq!(executions_probe.load(Ordering::SeqCst), 2);
    // 二版:收束不产任何机器文本。
    assert!(
        !reply.content.contains("Tool loop stopped") && !reply.content.contains("repeated"),
        "收束警告不该进正文: {}",
        reply.content
    );
    let turn = state.load_turns().unwrap().pop().expect("回合应当已落库");
    // 跳过轮回灌的是上一轮真实结果字节,不是错误提示。
    let replayed = turn
        .tool_flow
        .iter()
        .flat_map(|round| round.calls.iter())
        .filter(|call| call.output.contains("same results"))
        .count();
    assert!(replayed >= 3, "缓存结果应回灌到跳过轮, got {replayed}");
    // 轮数被钉在保险丝阈值附近(首轮+5 个相同轮+1 个无工具收束轮),
    // 不是桩服务器的 100。
    assert!(
        turn.tool_flow.len() <= 7,
        "tool_flow 有 {} 轮,复读闸没拦住",
        turn.tool_flow.len()
    );
    server.abort();
}

/// 平台受众(External)下保险丝/上限的机器面警告只进日志,绝不拼进正文
/// ——正文=群消息,拼进去就是把系统文本发到 QQ(08-24 线上翻车实录)。
#[tokio::test]
async fn repeat_fuse_warning_stays_out_of_external_reply_content() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = true;

    let mut tools = ToolRegistry::new();
    tools.register(ToolSpec::new(
        "web_search",
        "searches",
        empty_parameters(),
        |_| async { Ok("{\"ok\": true}".to_string()) },
    ));
    let control = AgentTurnControl::new(AgentMode::Normal, tools.clone(), tools.clone());

    let server = tokio::spawn(async move {
        for _ in 0..100 {
            let Ok((mut round, _)) = listener.accept().await else {
                return;
            };
            let _ = read_test_http_request(&mut round).await;
            write_test_sse(
                &mut round,
                concat!(
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"web_search\",\"arguments\":\"{\\\"query\\\":\\\"same\\\"}\"}}]}}]}\n\n",
                    "data: {\"choices\":[{\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n",
                    "data: [DONE]\n\n"
                ),
            )
            .await;
        }
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(config, &paths, state, client, tools, AgentMode::Normal).unwrap();
    agent.prompt_audience = crate::config::PromptAudience::External;

    let reply = agent
        .chat_stream_with_control("搜一下", &[], &control, |_| Ok(()))
        .await
        .unwrap();
    assert!(
        !reply.content.contains("Tool loop stopped"),
        "机器面警告漏进了平台正文: {}",
        reply.content
    );
    assert!(
        !reply.content.contains("reached the limit"),
        "上限警告漏进了平台正文: {}",
        reply.content
    );
    server.abort();
}

/// 回合内每次模型请求结束都发射 RoundUsage(provider 未报 usage 时走
/// 估算路径),这是 footer/WebUI 逐请求刷新计量的事件源。
#[tokio::test]
async fn round_usage_event_fires_per_model_request() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    let server = tokio::spawn(async move {
        let (mut chat, _) = listener.accept().await.unwrap();
        let _ = read_test_http_request(&mut chat).await;
        write_test_sse(
            &mut chat,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"回答\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":8,\"total_tokens\":128}}\n\n",
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
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let rounds = std::cell::RefCell::new(Vec::new());
    agent
        .chat_stream("你好", |event| {
            if let AgentEvent::RoundUsage {
                round,
                turn,
                estimated,
                ..
            } = &event
            {
                rounds
                    .borrow_mut()
                    .push((round.prompt_tokens, turn.total, *estimated));
            }
            Ok(())
        })
        .await
        .unwrap();
    let rounds = rounds.into_inner();
    assert_eq!(rounds.len(), 1);
    assert_eq!(rounds[0].0, 120);
    assert_eq!(rounds[0].1, 128);
    assert!(!rounds[0].2);
    server.await.unwrap();
}

/// keepalive 循环是 spawn 出去的独立任务，只认那个 AtomicBool。`Agent` 被丢掉
/// 时如果没人翻标志，它会继续按 interval 发请求——每次 ping 都是一次带完整
/// 前缀的 LLM 请求，**是真的在花钱**。
///
/// 原来只在「新回合开始」时取消，而每个平台回合用的是临时 Agent、跑完就丢，
/// 那条路上永远轮不到取消。
#[tokio::test]
async fn dropping_the_agent_stops_the_keepalive_loop() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    // 打开 keepalive：默认是 0（关闭），关着的话这条路根本不会起任务
    config.cache.keepalive_seconds = 3_600;
    config.cache.keepalive_max_pings = 20;

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    agent.seed_request_snapshot_for_test();
    agent.start_cache_keepalive();
    let cancel = agent
        .keepalive_cancel_flag()
        .expect("开了 keepalive 就该有取消标志");
    assert!(
        !cancel.load(std::sync::atomic::Ordering::Acquire),
        "刚起来不该是已取消"
    );

    drop(agent);

    assert!(
        cancel.load(std::sync::atomic::Ordering::Acquire),
        "Agent 被丢掉之后 keepalive 必须停——否则它会继续发请求计费"
    );
}

/// 量尺：`cargo test --lib agent::tests::stream::keepalive_snapshot_cost -- --ignored --nocapture`
///
/// 方案 M3 说「keepalive 快照持有整段会话（含 base64 图片）常驻，ping 和工具
/// 轮多次全量 clone」。量两件事：快照多大、克隆一次多贵。
#[test]
#[ignore]
fn keepalive_snapshot_cost() {
    use std::time::Instant;
    println!("\n  会话形态                    快照KB   单次clone(µs)   20次ping合计(ms)");
    let image = "A".repeat(100 * 1024); // 约 100KB 的 base64 图片
    for (label, turns, images) in [
        ("40 轮纯文本", 40usize, 0usize),
        ("40 轮 + 1 张图", 40, 1),
        ("40 轮 + 5 张图", 40, 5),
    ] {
        let mut messages = vec![ChatMessage::system("系统提示词".repeat(50))];
        for index in 0..turns {
            messages.push(ChatMessage::plain(
                "user",
                format!("问题 {index} ").repeat(20),
            ));
            messages.push(ChatMessage::assistant(
                format!("回答 {index} ").repeat(60),
                None,
            ));
        }
        for _ in 0..images {
            messages.push(ChatMessage::user_with_image(
                "看图",
                format!("data:image/png;base64,{image}"),
            ));
        }
        let bytes = serde_json::to_string(&messages).unwrap().len();
        for _ in 0..50 {
            std::hint::black_box(messages.clone());
        }
        let rounds = 500;
        let start = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(messages.clone());
        }
        let each_us = start.elapsed().as_secs_f64() * 1e6 / rounds as f64;
        println!(
            "  {label:<24}  {:>7}  {each_us:>13.1}  {:>16.2}",
            bytes / 1024,
            each_us * 20.0 / 1000.0
        );
    }
}
