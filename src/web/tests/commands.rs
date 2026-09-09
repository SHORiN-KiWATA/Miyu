//! WebUI 命令平面。

use super::shared::*;
use crate::web::*;

/// `GET /api/commands` 的返回形状是前端的契约：`commands.js` 拿 `name` 做精确
/// 匹配、拿 `arg_hint` 判断收不收参数、拿 `help` 渲染菜单第二列。任何一项缺了
/// 菜单就渲染不出来，而那是纯前端行为，Rust 侧看不见。
#[tokio::test]
async fn command_catalog_carries_what_the_menu_needs() {
    let temp = tempfile::tempdir().unwrap();
    let state = DaemonState::for_test(test_paths(temp.path()), 8300).unwrap();

    let response = list_commands(axum::extract::State(state), HeaderMap::new())
        .await
        .expect("未设密码时不该要鉴权");
    let commands = response.0["commands"].as_array().unwrap().clone();

    assert!(!commands.is_empty(), "一条命令都没返回，前端菜单是空的");
    for command in &commands {
        let name = command["name"].as_str().expect("缺 name");
        assert!(name.starts_with('/'), "{name} 不是斜杠命令");
        assert!(
            command["arg_hint"].is_string(),
            "{name} 缺 arg_hint——前端据它判断收不收参数"
        );
        assert!(
            command["help"]
                .as_str()
                .is_some_and(|help| !help.is_empty()),
            "{name} 缺帮助文案"
        );
    }

    // 与 `web_commands()` 同源：这里返回的必须正好是打了 web 标记的那批。
    let expected = crate::slash_commands::web_commands()
        .into_iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();
    let actual = commands
        .iter()
        .map(|command| command["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

/// `/reset-all-memory` 走 WebUI 时必须真的清掉那份记忆，且 dev 与普通模式各清
/// 各的——dev 的记忆挂在保留人格名下，钥匙不对就清的是另一份（与
/// `IpcCommand::ResetMemory` 同一个坑）。
#[tokio::test]
async fn web_memory_reset_all_clears_the_mode_it_was_asked_for() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let state = DaemonState::for_test(paths.clone(), 8301).unwrap();
    let config = state.manager.lock().unwrap().config.clone();

    let normal = crate::memory::MemoryStore::new(&config, &paths);
    let dev = crate::memory::MemoryStore::new(&config.dev_scoped(), &paths);
    normal
        .remember_fact("普通模式记住 XMODIFIERS 这件事", "test")
        .unwrap();
    dev.remember_fact("开发模式记住 XMODIFIERS 这件事", "test")
        .unwrap();
    let recalled = |store: &crate::memory::MemoryStore| {
        store
            .recall_memories("XMODIFIERS", 5, false)
            .unwrap()
            .to_string()
    };
    assert!(recalled(&normal).contains("普通模式"));
    assert!(recalled(&dev).contains("开发模式"));

    let response = reset_all_memory_http(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        axum::Json(serde_json::from_value(serde_json::json!({ "mode": "dev" })).unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(response.0["ok"], true);

    assert!(
        !recalled(&dev).contains("开发模式"),
        "点名清 dev，dev 的记忆却还在"
    );
    assert!(
        recalled(&normal).contains("普通模式"),
        "只该清 dev，普通模式的记忆被误伤了"
    );
}

/// `/reset-memory` 清的是**发命令的那个会话**。WebUI 能同时开好几个会话，
/// daemon 的全局指针未必是它，所以请求里带的 session_id 必须说了算——不然
/// 用户在 A 会话敲的命令会把 B 会话的记忆清掉。
#[tokio::test]
async fn web_memory_reset_clears_only_the_session_it_was_asked_for() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let state = DaemonState::for_test(paths.clone(), 8302).unwrap();
    let config = state.manager.lock().unwrap().config.clone();

    let store = crate::memory::MemoryStore::new(&config, &paths);
    store
        .clone()
        .with_session_id("session-a")
        .remember_fact("A 会话记住 XMODIFIERS 这件事", "test")
        .unwrap();
    store
        .clone()
        .with_session_id("session-b")
        .remember_fact("B 会话记住 XMODIFIERS 这件事", "test")
        .unwrap();
    let recalled = || {
        store
            .recall_memories("XMODIFIERS", 5, false)
            .unwrap()
            .to_string()
    };
    assert!(recalled().contains("A 会话"));
    assert!(recalled().contains("B 会话"));

    let response = reset_memory_http(
        axum::extract::State(state.clone()),
        HeaderMap::new(),
        axum::Json(
            serde_json::from_value(serde_json::json!({ "session_id": "session-a" })).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.0["ok"], true);
    assert!(response.0["text"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));

    assert!(!recalled().contains("A 会话"), "点名清 A，A 的记忆却还在");
    assert!(
        recalled().contains("B 会话"),
        "只该清 A，B 会话的记忆被误伤了"
    );
}

/// 回合的工具记录必须出现在发给 WebUI 的 payload 里。
///
/// 以前不发：`tool_flow` 一直躺在库里，但 DTO 没这个字段，于是 WebUI 的工具
/// 信息只在实时事件流里活过一次——切走再切回来就没了，而库里明明有。
#[test]
fn turn_payload_carries_the_tools_that_ran() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let store = StateStore::new(&paths).unwrap();
    store.init_files().unwrap();
    store
        .start_turn("t1", "跑一下 ls", std::process::id())
        .unwrap();
    store
        .set_turn_tool_flow(
            "t1",
            &[crate::state::ToolFlowRound {
                remote: false,
                assistant_content: String::new(),
                assistant_reasoning: None,
                calls: vec![crate::state::ToolFlowCall {
                    id: "call_1".to_string(),
                    name: "run_command".to_string(),
                    arguments: r#"{"command":"ls"}"#.to_string(),
                    output: "a.txt\nb.txt".to_string(),
                }],
            }],
        )
        .unwrap();
    store.complete_turn("t1", "跑完了", None).unwrap();

    let turn = store.load_turns().unwrap().remove(0);
    let payload = serde_json::to_value(SafeTurn::from_turn(turn, Vec::new(), Vec::new())).unwrap();
    let calls = payload["tool_flow"][0]["calls"].as_array().unwrap();
    assert_eq!(calls[0]["name"], "run_command");
    assert!(
        calls[0]["output"]
            .as_str()
            .is_some_and(|out| out.contains("a.txt")),
        "工具输出没进 payload，切回会话就看不到了"
    );
}
