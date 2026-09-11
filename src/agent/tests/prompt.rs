//! 系统提示词的组装与字节稳定性。

use super::shared::*;
use crate::agent::*;
use crate::config::AppConfig;
use crate::platforms::{ConversationKind, PlatformConversation};
use tokio::net::TcpListener;

#[test]
fn runtime_context_contains_dynamic_runtime_only() {
    let context = runtime_context(AgentMode::Normal, false);
    assert!(context.starts_with("<runtime "));
    assert!(context.contains("now=\""));
    assert!(context.contains("cwd=\""));
    for noise in ["env=", "shell=", "terminal=", "note="] {
        assert!(!context.contains(noise), "{noise} in {context}");
    }
    // ISO 日期 + 三字母星期,不是中文长日期。
    assert!(!context.contains('年'), "{context}");
}

#[test]
fn a_platform_runtime_stamp_carries_nothing_a_chat_message_cannot_use() {
    // A QQ turn has no working directory, no shell and no terminal. Those
    // attributes were re-sent at full price on every single turn — 285
    // chars where a timestamp needs about 45.
    let platform = runtime_context(AgentMode::Normal, true);
    assert!(platform.contains("now=\""), "{platform}");
    for noise in ["cwd=", "shell=", "terminal=", "env=", "note="] {
        assert!(!platform.contains(noise), "{noise} in {platform}");
    }
    // 平台面到分钟,终端面到小时:同粒度内整块字节不变。
    assert!(platform.contains(':'), "{platform}");
    let terminal = runtime_context(AgentMode::Normal, false);
    let stamp = terminal
        .split("now=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap();
    // 终端面到小时:分钟位恒为 00,同一小时内整块字节不变。
    assert!(stamp.ends_with(":00"), "{stamp}");
}

#[test]
fn host_environment_rides_the_system_prompt_for_owners_only() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());

    let owner = with_host_environment(
        "base".to_string(),
        PromptAudience::Owner,
        &paths,
        &AppConfig::default(),
        AgentMode::Normal,
        false,
    );
    assert!(owner.starts_with("base\n\n<host-environment os=\""));
    assert!(owner.contains("/>"));
    assert!(owner.contains("LaTeX"), "渲染能力说明应跟随 owner 提示词");
    assert!(owner.contains(&format!(" miyu_home=\"{}\"", paths.root_dir.display())));
    // The static block must not be mistaken for the per-turn stamp, and
    // `mode_reminder_does_not_inject_a_reasoning_title_protocol` asserts the
    // system prompt never carries a `<runtime` tag.
    assert!(!owner.contains("<runtime"));

    // 判官/子代理(Internal)一字不加;平台会话(External)只多一段风格锁——
    // 它与受众无关,主机路径、LaTeX、语音协议仍旧只给属主。
    assert_eq!(
        with_host_environment(
            "base".to_string(),
            PromptAudience::Internal,
            &paths,
            &AppConfig::default(),
            AgentMode::Normal,
            false,
        ),
        "base"
    );
    let external = with_host_environment(
        "base".to_string(),
        PromptAudience::External,
        &paths,
        &AppConfig::default(),
        AgentMode::Normal,
        true,
    );
    assert_eq!(external, format!("base{STYLE_LOCK}"));
    assert!(!external.contains("<host-environment"));
    assert!(!external.contains("LaTeX"));
    assert!(!external.contains("<voice-protocol"));
    // WebUI 回合(External 但不是平台回合):带主机环境块,但 LaTeX/语音协议仍只给属主
    let webui = with_host_environment(
        "base".to_string(),
        PromptAudience::External,
        &paths,
        &AppConfig::default(),
        AgentMode::Normal,
        false,
    );
    assert!(webui.starts_with("base\n\n<host-environment os=\""));
    assert!(webui.ends_with(STYLE_LOCK));
    assert!(!webui.contains("LaTeX"));
    // dev 提示词极简,外部受众也不带风格锁。
    assert_eq!(
        with_host_environment(
            "base".to_string(),
            PromptAudience::External,
            &paths,
            &AppConfig::default(),
            AgentMode::Dev,
            true,
        ),
        "base"
    );
    // 属主提示词的字节顺序不变:风格锁仍在主机环境之后。
    let host_at = owner.find("<host-environment").unwrap();
    let lock_at = owner.find("<style-lock>").unwrap();
    assert!(host_at < lock_at);
}

#[test]
fn host_environment_is_byte_stable_across_prompt_rebuilds() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    // Rebuilt on every turn by `prepare_for_turn`; a value that drifted
    // between rebuilds would move the prefix and cost a cache miss a turn.
    let first = with_host_environment(
        String::new(),
        PromptAudience::Owner,
        &paths,
        &AppConfig::default(),
        AgentMode::Normal,
        false,
    );
    let second = with_host_environment(
        String::new(),
        PromptAudience::Owner,
        &paths,
        &AppConfig::default(),
        AgentMode::Normal,
        false,
    );
    assert_eq!(first, second);
}

#[test]
fn user_identity_is_limited_to_owner_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    let mut config = AppConfig::default();
    std::fs::create_dir_all(config.identities_dir_path(&paths)).unwrap();
    std::fs::write(config.user_identity_path(&paths), "legacy-owner-marker").unwrap();

    let owner = config
        .system_prompt_for(&paths, PromptAudience::Owner)
        .unwrap();
    let external = config
        .system_prompt_for(&paths, PromptAudience::External)
        .unwrap();
    let internal = config
        .system_prompt_for(&paths, PromptAudience::Internal)
        .unwrap();
    assert!(owner.contains("legacy-owner-marker"));
    assert!(!external.contains("legacy-owner-marker"));
    assert!(!internal.contains("legacy-owner-marker"));

    config.prompt.active_identity = "owner.md".to_string();
    std::fs::write(
        config.identity_path(&paths, "owner.md"),
        "active-owner-marker",
    )
    .unwrap();
    assert!(config
        .system_prompt_for(&paths, PromptAudience::Owner)
        .unwrap()
        .contains("active-owner-marker"));
    assert!(!config
        .system_prompt_for(&paths, PromptAudience::External)
        .unwrap()
        .contains("active-owner-marker"));
}

#[test]
fn runtime_system_context_refreshes_the_effective_prompt_immediately() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();

    agent
        .set_runtime_system_context(vec!["  platform-only notice  ".to_string()])
        .unwrap();
    assert!(agent.system_prompt.contains("platform-only notice"));
    assert_eq!(
        agent.runtime_system_context,
        vec!["platform-only notice".to_string()]
    );
}

#[test]
fn nothing_after_the_leading_prompt_may_carry_the_system_role() {
    // Provider chat templates gather every `system` message to the front of
    // the rendered prompt, so one appearing mid-conversation shifts that
    // block and drops the prefix cache to zero. Measured on DeepSeek with a
    // byte-identical prefix: appending `assistant + user` hit 99%, the same
    // append with one `system` in front of it hit 0%, and moving that
    // `system` to the very end still hit 0%.
    let messages = vec![
        ChatMessage::system("persona"),
        ChatMessage::plain("user", "问题"),
        ChatMessage::turn_context("<runtime now=\"x\"/>"),
        ChatMessage::turn_context("<associative-memory>x</associative-memory>"),
        ChatMessage::assistant("答案", None),
    ];
    let stray: Vec<usize> = messages
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, message)| message.role == "system")
        .map(|(index, _)| index)
        .collect();
    assert!(
        stray.is_empty(),
        "system role at {stray:?} would reset the prefix cache"
    );
}

/// 防失忆提醒(08-16 版):首回合蒸馏后以化石身份进历史;间隔轮数内
/// 的第二回合不再注入新份——请求里只有回放的那一份,且当前轮尾部
/// 干净(runtime 投影同小时也跳注入),前缀纯追加。
#[tokio::test]
async fn persona_reminder_fossilizes_on_interval_and_replays() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    config.system_prompt = Some("测试人格：说话简短。".to_string());
    config.prompt.persona_reminder = true;

    let (first_chat_tx, first_chat_rx) = oneshot::channel();
    let (second_chat_tx, second_chat_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let reply = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"哦\"}}]}\n\n",
            "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
            "data: [DONE]\n\n"
        );
        // 回合1请求①:蒸馏调用(产物首行名字,次行正文)。
        let (mut distill, _) = listener.accept().await.unwrap();
        let request = read_test_http_request(&mut distill).await;
        let body: serde_json::Value = serde_json::from_slice(&request).unwrap();
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("persona definition file"));
        write_test_sse(
            &mut distill,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"短\\n回复很短，从不用Emoji。\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
        // 回合1请求②:正式对话。
        let (mut chat, _) = listener.accept().await.unwrap();
        let _ = first_chat_tx.send(read_test_http_request(&mut chat).await);
        write_test_sse(&mut chat, reply).await;
        // 回合2请求①:缓存命中,直接就是对话请求(若再蒸馏一次,
        // 这里读到的请求不含新消息,下方断言会失败)。
        let (mut chat2, _) = listener.accept().await.unwrap();
        let _ = second_chat_tx.send(read_test_http_request(&mut chat2).await);
        write_test_sse(&mut chat2, reply).await;
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config.clone(),
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let context = Arc::new(PlatformTurnContext::new(
        PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "20000".to_string(),
        },
        "30000".to_string(),
        "tester".to_string(),
        false,
        config,
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        Arc::new(NoopPlatformAdapter),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));
    agent.set_platform_context_images(context.clone(), Vec::new());
    agent.chat_stream("第一条消息", |_| Ok(())).await.unwrap();

    let expected_reminder = "<persona-reminder>回复很短，从不用Emoji。\
         就算是讲解答疑，也只说最关键的两三步，整条不超过一百字，\
         一次说不完就等对方追问。</persona-reminder>";
    let request: serde_json::Value = serde_json::from_slice(&first_chat_rx.await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    // 提醒以化石身份入列(位置在 runtime 之后、随机注入的表情包
    // 提醒之前),不再断言绝对末尾——只断言恰好一份。
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["content"] == expected_reminder)
            .count(),
        1
    );
    let turns = state.load_turns().unwrap();
    // 新语义:提醒就是化石,回放历史自带。
    assert!(format!("{:?}", turns[0].context_messages).contains("persona-reminder"));
    assert!(paths
        .state_dir
        .join("persona-hints")
        .read_dir()
        .unwrap()
        .next()
        .is_some());

    agent.set_platform_context_images(context, Vec::new());
    agent.chat_stream("第二条消息", |_| Ok(())).await.unwrap();
    let request: serde_json::Value =
        serde_json::from_slice(&second_chat_rx.await.unwrap()).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["content"]
            .as_str()
            .is_some_and(|content| content.contains("第二条消息"))
    }));
    let reminder_count = messages
        .iter()
        .filter(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains("persona-reminder"))
        })
        .count();
    // 间隔(默认3)未到:仅回放化石那一份,不再追加新份;绝对末尾
    // 不再是漂浮提醒(可能是用户消息或跨分钟的新 runtime,都合法)。
    assert_eq!(reminder_count, 1);
    assert!(messages
        .iter()
        .any(|message| message["content"] == expected_reminder));
    assert_ne!(messages.last().unwrap()["content"], expected_reminder);
    server.await.unwrap();
}

/// 手写防失忆提示(hints/<scope>.md)优先于自动蒸馏:存在时整回合
/// 不发蒸馏请求(服务端只应答一次对话),尾部原样携带手写内容,
/// 不拼场景句。
#[tokio::test]
async fn manual_persona_reminder_overrides_distillation() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut config = queue_test_config(base_url);
    config.tools.enabled = false;
    config.system_prompt = Some("测试人格：说话简短。".to_string());
    config.prompt.persona_reminder = true;
    let hint_path = crate::persona_hint::manual_hint_path(&config, &paths, "default");
    std::fs::create_dir_all(hint_path.parent().unwrap()).unwrap();
    std::fs::write(&hint_path, "未有在群里潜水。手写版提醒。\n").unwrap();

    let (chat_tx, chat_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut chat, _) = listener.accept().await.unwrap();
        let _ = chat_tx.send(read_test_http_request(&mut chat).await);
        write_test_sse(
            &mut chat,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"哦\"}}]}\n\n",
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
        config.clone(),
        &paths,
        state.clone(),
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let context = Arc::new(PlatformTurnContext::new(
        PlatformConversation {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Group,
            conversation_id: "20000".to_string(),
        },
        "30000".to_string(),
        "tester".to_string(),
        false,
        config,
        paths.clone(),
        StateStore::new(&paths).unwrap(),
        Arc::new(NoopPlatformAdapter),
        Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
    ));
    agent.set_platform_context_images(context, Vec::new());
    agent.chat_stream("第一条消息", |_| Ok(())).await.unwrap();

    let request: serde_json::Value = serde_json::from_slice(&chat_rx.await.unwrap()).unwrap();
    let last = request["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(
        last["content"],
        "<persona-reminder>未有在群里潜水。手写版提醒。</persona-reminder>"
    );
    server.await.unwrap();
}

/// 预设对话(begin_dialogs):system 之后、真实历史之前注入 Q/A 对,
/// 每请求从 dialogs/<scope>.md 重建、永不落库。
#[test]
fn preset_dialogs_ride_after_system_before_history() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let dialogs = crate::persona_hint::dialogs_path(&config, &paths, "default");
    std::fs::create_dir_all(dialogs.parent().unwrap()).unwrap();
    std::fs::write(&dialogs, "user: 你好\nassistant: 哼，又来一个。\n").unwrap();
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    state.start_turn("turn_h", "历史问题", 999999).unwrap();
    state.complete_turn("turn_h", "历史回答", None).unwrap();
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
    let messages = agent.chat_messages("current", "新消息").unwrap().0;
    assert_eq!(messages[0].role, "system");
    assert_eq!(messages[1].role, "user");
    assert_eq!(chat_message_text(&messages[1]).unwrap(), "你好");
    assert_eq!(messages[2].role, "assistant");
    assert_eq!(chat_message_text(&messages[2]).unwrap(), "哼，又来一个。");
    assert_eq!(chat_message_text(&messages[3]).unwrap(), "历史问题");
    // 预设对话只活在请求里:历史存储不含它。
    let turns = agent.state.load_turns().unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].user_content, "历史问题");
}

/// Dev 模式极简组装:系统提示词是 dev-prompt.md 的一行(缺省内置默认),
/// 人格全家(预设对话/用户档案)整套绕开——即使 dialogs 文件存在。
#[test]
fn dev_mode_uses_one_line_prompt_and_skips_persona_family() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    // 人格侧的预设对话文件在场,dev 也必须无视。
    let dialogs = crate::persona_hint::dialogs_path(&config, &paths, "default");
    std::fs::create_dir_all(dialogs.parent().unwrap()).unwrap();
    std::fs::write(&dialogs, "user: 你好\nassistant: 哼，又来一个。\n").unwrap();
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    state.start_turn("turn_h", "历史问题", 999999).unwrap();
    state.complete_turn("turn_h", "历史回答", None).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Dev,
    )
    .unwrap();
    let messages = agent.chat_messages("current", "新消息").unwrap().0;
    assert_eq!(messages[0].role, "system");
    let system = chat_message_text(&messages[0]).unwrap();
    assert!(
        system.contains(crate::config::DEFAULT_DEV_SYSTEM_PROMPT),
        "dev 系统提示词应为内置默认一行: {system}"
    );
    assert!(!system.contains("<current-user-profile>"), "dev 无用户身份");
    // 09-09:记忆整套退场,连 `<associative-memory>` 前言都不该出现。
    assert!(
        !system.contains("<associative-memory>"),
        "dev 不带记忆,前言不该进 system: {system}"
    );
    // 第一条对话消息直接是历史,没有预设对话对。
    assert_eq!(messages[1].role, "user");
    assert_eq!(chat_message_text(&messages[1]).unwrap(), "历史问题");
}

/// `load_tools` 在 full 档里是死重量:模型看不见它就不会调,而它每轮都占
/// 着目录。留它的唯一理由是「历史里已有调用记录时不能变成未知工具」——
/// 所以判据是本会话到底调没调过,而不是档位本身。
#[test]
fn dev_load_tools_registers_only_after_the_session_used_it() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let tools = crate::tools::build_tool_registry(&config, &paths, AgentMode::Dev, false).unwrap();
    assert!(tools.contains("load_tools"), "底座表里本来就有 load_tools");
    let mut agent =
        Agent::new(config, &paths, state.clone(), client, tools, AgentMode::Dev).unwrap();

    agent.prepare_for_turn().unwrap();
    assert!(
        !agent.tools.lock().unwrap().contains("load_tools"),
        "full 档 + 全新会话:不该带 load_tools"
    );

    // 会话里出现过加载记录(从需加载档切过来的会话就是这个形状)。
    state
        .add_session_loaded_tools(&["web_search".to_string()], None)
        .unwrap();
    agent.prepare_for_turn().unwrap();
    assert!(
        agent.tools.lock().unwrap().contains("load_tools"),
        "历史里调用过就必须放回来,否则模型照着历史撞未知工具"
    );
}
