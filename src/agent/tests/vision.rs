//! 视觉能力的判定与图片投递。

use super::shared::*;
use crate::agent::*;
use crate::config::{ActiveProviderModelConfig, AppConfig, ProviderConfig};
use crate::platforms::{ConversationKind, PlatformConversation};
use std::path::PathBuf;
use tokio::net::TcpListener;

#[test]
fn vision_support_requires_every_effective_text_pool_model() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    provider.default_model = "vision-model".to_string();
    provider.models = vec!["vision-model".to_string(), "text-model".to_string()];
    provider.model_modalities.insert(
        "vision-model".to_string(),
        vec!["text".to_string(), "image".to_string()],
    );
    provider
        .model_modalities
        .insert("text-model".to_string(), vec!["text".to_string()]);
    let provider_id = provider.id.clone();

    config.active_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: provider_id.clone(),
        model: "vision-model".to_string(),
    }]);
    assert!(active_text_pool_supports_vision(&config));

    config
        .active_provider_models
        .as_mut()
        .unwrap()
        .push(ActiveProviderModelConfig {
            provider_id,
            model: "text-model".to_string(),
        });
    assert!(!active_text_pool_supports_vision(&config));
}

#[test]
fn vision_preference_controls_direct_image_delivery_to_the_text_pool() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    provider.model_modalities.insert(
        provider.default_model.clone(),
        vec!["text".to_string(), "image".to_string()],
    );

    assert!(should_use_active_text_pool_for_images(&config));
    config.plugins.vision.prefer_current_multimodal_model = false;
    assert!(!should_use_active_text_pool_for_images(&config));
}

/// agy 中转线的模型目录里 Gemini 标着 image 输入,但 stdin 只收文本:图不能
/// 内联进消息,只能留路径让模型自己 view_file(09-04 实测)。判成"能看图"
/// 的后果是图被中转层降级丢掉、活体与化石字节不同、续传链逢图必断。
#[test]
fn antigravity_pool_never_inlines_media_but_views_it_natively() {
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| provider.is_antigravity())
        .unwrap();
    provider.enabled = true;
    let model = provider.default_model.clone();
    provider
        .model_modalities
        .insert(model.clone(), vec!["text".to_string(), "image".to_string()]);
    let provider_id = provider.id.clone();
    config.active_provider_models = Some(vec![ActiveProviderModelConfig { provider_id, model }]);

    assert!(!active_text_pool_supports_vision(&config));
    assert!(!should_use_active_text_pool_for_images(&config));
    assert!(config.active_pool_views_media_with_native_file_tool(false));
    // 原生工具在本模式关着 ⇒ 没人能打开路径,退回视觉旁路。
    config.plugins.antigravity.native_tools = "dev".to_string();
    assert!(!config.active_pool_views_media_with_native_file_tool(false));
    assert!(config.active_pool_views_media_with_native_file_tool(true));
}

#[tokio::test]
async fn platform_images_register_a_turn_scoped_vision_tool() {
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
    agent.set_image_platform("qq", "QQ");
    let images = vec![Some(PastedImage::Binary(ClipboardImage::new(
        "image/png".to_string(),
        vec![1, 2, 3],
    )))];

    let prepared = agent.prepare_user_input("看图", &images).await.unwrap();
    let hint = format!("{:?}", prepared.hints);
    assert!(hint.contains("vision_analyze"));
    let tools = agent.tools.lock().unwrap().clone();
    assert!(tools.contains("vision_analyze"));
    let error = tools
        .call("vision_analyze", r#"{"image":"/etc/passwd"}"#)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("image is not attached to the current platform turn"));
}

#[tokio::test]
async fn context_image_ids_register_vision_without_a_current_image() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let config = AppConfig::default();
    let state = StateStore::new(&paths).unwrap();
    let client =
        OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths).unwrap();
    let mut agent = Agent::new(
        config.clone(),
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    agent.set_image_platform("qq", "QQ");
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
    agent.set_platform_context_images(
        context,
        vec![PlatformContextImageRef {
            id: "context_image_1".to_string(),
            message_id: "90".to_string(),
            image_index: 1,
        }],
    );

    let prepared = agent.prepare_user_input("接着说", &[]).await.unwrap();
    assert!(format!("{:?}", prepared.hints).contains("context_image_1"));
    let tools = agent.tools.lock().unwrap();
    assert!(tools.contains("vision_analyze"));
    let definition = tools
        .definitions()
        .into_iter()
        .find(|definition| definition.function.name == "vision_analyze")
        .unwrap();
    assert!(definition.function.description.contains("context_image_N"));
}

#[tokio::test]
async fn binary_image_reaches_vision_pool_then_text_model() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let vision_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let text_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config =
        queue_test_config(format!("http://{}/v1", text_listener.local_addr().unwrap()));
    config.tools.enabled = false;
    config.plugins.vision.enabled = true;
    config.providers.push(ProviderConfig {
        enabled: true,
        id: "vision-test".to_string(),
        display_name: "Vision Test".to_string(),
        base_url: format!("http://{}/v1", vision_listener.local_addr().unwrap()),
        protocol: "openai-chat".to_string(),
        api_key: Some("test-key".to_string()),
        models: vec!["vision-model".to_string()],
        model_context_window: Default::default(),
        model_temperature: HashMap::new(),
        model_tools_loading_mode: HashMap::new(),
        model_modalities: [(
            "vision-model".to_string(),
            vec!["text".to_string(), "image".to_string()],
        )]
        .into(),
        tool_result_media: None,
        model_costs: Default::default(),
        default_model: "vision-model".to_string(),
        timeout_seconds: 30,
        temperature: 0.0,
        anthropic_max_tokens: 4096,
        extra_body: None,
    });
    config.active_multimodal_provider_models = Some(vec![ActiveProviderModelConfig {
        provider_id: "vision-test".to_string(),
        model: "vision-model".to_string(),
    }]);

    let (vision_request_tx, vision_request_rx) = oneshot::channel();
    let vision_server = tokio::spawn(async move {
        let (mut stream, _) = vision_listener.accept().await.unwrap();
        let request = read_test_http_request(&mut stream).await;
        let _ = vision_request_tx.send(request);
        write_test_sse(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"a red square\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });
    let (text_request_tx, text_request_rx) = oneshot::channel();
    let text_server = tokio::spawn(async move {
        let (mut stream, _) = text_listener.accept().await.unwrap();
        let request = read_test_http_request(&mut stream).await;
        let _ = text_request_tx.send(request);
        write_test_sse(
            &mut stream,
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"I can see it.\"}}]}\n\n",
                "data: {\"choices\":[{\"finish_reason\":\"stop\",\"delta\":{}}]}\n\n",
                "data: [DONE]\n\n"
            ),
        )
        .await;
    });

    let state = StateStore::new(&paths).unwrap();
    state.init_files().unwrap();
    let text_provider = config.provider(None).unwrap().clone();
    let client = OpenAiCompatibleClient::new(&text_provider, &config, &paths).unwrap();
    let mut agent = Agent::new(
        config,
        &paths,
        state,
        client,
        ToolRegistry::new(),
        AgentMode::Normal,
    )
    .unwrap();
    let image = PastedImage::Binary(ClipboardImage::new(
        "image/png".to_string(),
        b"qq-image-bytes".to_vec(),
    ));

    let result = agent
        .chat_stream_with_images("What is shown?", &[Some(image)], |_| Ok(()))
        .await
        .unwrap();

    assert_eq!(result.content, "I can see it.");
    let vision_request: Value = serde_json::from_slice(&vision_request_rx.await.unwrap()).unwrap();
    let vision_parts = vision_request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_array()
        .unwrap();
    assert!(vision_parts.iter().any(|part| {
        part["type"] == "image_url"
            && part["image_url"]["url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
    }));

    let text_request: Value = serde_json::from_slice(&text_request_rx.await.unwrap()).unwrap();
    let serialized = serde_json::to_string(&text_request).unwrap();
    assert!(serialized.contains("What is shown?"));
    assert!(serialized.contains("a red square"));
    vision_server.await.unwrap();
    text_server.await.unwrap();
}

#[test]
fn binary_image_cache_is_isolated_by_platform() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let images = vec![Some(PastedImage::Binary(ClipboardImage::new(
        "image/jpeg".to_string(),
        b"same-image-content".to_vec(),
    )))];

    let platform = resolve_pasted_image_paths(&images, &paths, Some("qq"));
    let platform_path = PathBuf::from(platform[0].as_deref().unwrap());
    assert!(platform_path.starts_with(paths.cache_dir.join("platform_images/qq")));
    assert!(platform_path.is_file());

    let clipboard = resolve_pasted_image_paths(&images, &paths, None);
    let clipboard_path = PathBuf::from(clipboard[0].as_deref().unwrap());
    assert!(clipboard_path.starts_with(paths.cache_dir.join("clipboard_images")));
    assert!(clipboard_path.is_file());
    assert_ne!(platform_path, clipboard_path);
}

/// Path 形态图片(shell-hook/粘贴路径)在视觉模型在场时内联直读,与
/// Binary 对齐;路径读不出来则不产 image part,落回纯文本+工具提示。
/// 退回 input.rs 的 inline_path_urls 前,第一段断言会报红。
#[tokio::test]
async fn path_images_are_inlined_when_model_supports_vision() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let mut config = AppConfig::default();
    let provider = config
        .providers
        .iter_mut()
        .find(|provider| !provider.is_builtin_cli_provider())
        .unwrap();
    provider.model_modalities.insert(
        provider.default_model.clone(),
        vec!["text".to_string(), "image".to_string()],
    );
    let image_path = temp.path().join("shot.png");
    std::fs::write(&image_path, b"fake png bytes").unwrap();

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

    let prepared = agent
        .prepare_user_input(
            "看看这张图 [Image 1]",
            &[Some(crate::clipboard::PastedImage::Path(
                image_path.display().to_string(),
            ))],
        )
        .await
        .unwrap();
    let parts = match prepared.message.content.as_ref() {
        Some(ChatContent::Parts(parts)) => parts,
        other => panic!("expected parts message, got {other:?}"),
    };
    assert!(parts.iter().any(|part| matches!(
        part,
        ChatContentPart::ImageUrl { image_url } if image_url.url.starts_with("data:image/png;base64,")
    )));

    // 路径不存在:不内联,退回纯文本消息。
    let prepared = agent
        .prepare_user_input(
            "看看这张图 [Image 1]",
            &[Some(crate::clipboard::PastedImage::Path(
                temp.path().join("missing.png").display().to_string(),
            ))],
        )
        .await
        .unwrap();
    assert!(matches!(
        prepared.message.content.as_ref(),
        Some(ChatContent::Text(_))
    ));
}

/// 视频当附件内联进主对话(08-27 用户要求"当附件")。
///
/// 判据与图片各管各的:模型吃视频才内联,不吃就原样留给 `vision_analyze`——
/// 而已经内联的那些不该再提示去调工具,否则白烧一次旁路请求。
#[tokio::test]
async fn local_video_paths_inline_when_the_model_takes_video() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let video = temp.path().join("clip.mp4");
    std::fs::write(&video, b"\x00\x00\x00\x18ftypmp42").unwrap();
    let video_path = video.to_string_lossy().to_string();

    let build = |modalities: Vec<String>| {
        let mut config = AppConfig::default();
        let provider_id = config.provider(None).unwrap().id.clone();
        let model = config.provider(None).unwrap().default_model.clone();
        for provider in &mut config.providers {
            if provider.id == provider_id {
                provider
                    .model_modalities
                    .insert(model.clone(), modalities.clone());
            }
        }
        config.active_provider_models = Some(vec![crate::config::ActiveProviderModelConfig {
            provider_id,
            model,
        }]);
        config
    };

    let video_parts = |prepared: &crate::agent::PreparedUserInput| match &prepared.message.content {
        Some(crate::llm::ChatContent::Parts(parts)) => parts
            .iter()
            .filter(|part| matches!(part, crate::llm::ChatContentPart::VideoUrl { .. }))
            .count(),
        _ => 0,
    };

    // 吃视频:内联,且不再提示走工具。
    let config = build(vec!["text".into(), "image".into(), "video".into()]);
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
    let images = vec![Some(PastedImage::Path(video_path.clone()))];
    let prepared = agent.prepare_user_input("看看这段", &images).await.unwrap();
    assert_eq!(video_parts(&prepared), 1, "吃视频的模型应当内联视频块");
    let hints = format!("{:?}", prepared.hints);
    assert!(!hints.contains("本地视频路径"), "已内联就别再叫它调工具");

    // 只因为带了视频就走进内联分支时,图片这一份仍要单独受视觉能力约束:
    // `prefer_current_multimodal_model` 关掉后,图片本该由视觉插件描述进正文,
    // 再把原图塞进 parts 就是同一批图处理两遍,还发给了刚判定"不该收图"的
    // 模型(08-27 自审抓到)。
    let mut config = build(vec!["text".into(), "image".into(), "video".into()]);
    config.plugins.vision.prefer_current_multimodal_model = false;
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
    assert!(!agent.current_model_supports_vision());
    let mixed = vec![
        Some(PastedImage::Path(video_path.clone())),
        Some(PastedImage::Binary(ClipboardImage::new(
            "image/png".to_string(),
            vec![0x89, b'P', b'N', b'G'],
        ))),
    ];
    let prepared = agent.prepare_user_input("一起看看", &mixed).await.unwrap();
    assert_eq!(video_parts(&prepared), 1, "视频仍应内联");
    let image_parts = match &prepared.message.content {
        Some(crate::llm::ChatContent::Parts(parts)) => parts
            .iter()
            .filter(|part| matches!(part, crate::llm::ChatContentPart::ImageUrl { .. }))
            .count(),
        _ => 0,
    };
    assert_eq!(image_parts, 0, "不该收图的模型不能因为带了视频就收到图片块");

    // 不吃视频:不内联,留给工具。
    let config = build(vec!["text".into(), "image".into()]);
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
    let images = vec![Some(PastedImage::Path(video_path))];
    let prepared = agent.prepare_user_input("看看这段", &images).await.unwrap();
    assert_eq!(video_parts(&prepared), 0, "不吃视频的模型不该收到视频块");
}

/// 已内联的图片不该再邀请模型去调视觉工具(08-27 用户点名"多模态模型总去调
/// 视觉分析工具")。
///
/// 病灶不在工具、在提示:两处 hint 都不看图有没有已经内联,于是多模态模型明明
/// 看得见图,同一轮还会收到一句"你可以用 vision_analyze 分析这些图片",它照做
/// 就成了白烧一次旁路请求。拦工具是过滤症状,撤掉这句邀请才是根因。
#[tokio::test]
async fn inlined_images_do_not_invite_the_vision_tool() {
    let temp = tempfile::tempdir().unwrap();
    let paths = test_paths(temp.path());
    let picture = temp.path().join("shot.png");
    std::fs::write(&picture, b"\x89PNG\r\n\x1a\n").unwrap();
    let picture_path = picture.to_string_lossy().to_string();

    let build = |modalities: Vec<String>| {
        let mut config = AppConfig::default();
        let provider_id = config.provider(None).unwrap().id.clone();
        let model = config.provider(None).unwrap().default_model.clone();
        for provider in &mut config.providers {
            if provider.id == provider_id {
                provider
                    .model_modalities
                    .insert(model.clone(), modalities.clone());
            }
        }
        config.active_provider_models = Some(vec![crate::config::ActiveProviderModelConfig {
            provider_id,
            model,
        }]);
        config
    };
    let prepare_binary = |config: AppConfig, images: Vec<Option<PastedImage>>| {
        let paths = paths.clone();
        async move {
            let state = StateStore::new(&paths).unwrap();
            let client =
                OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths)
                    .unwrap();
            let agent = Agent::new(
                config,
                &paths,
                state,
                client,
                crate::tools::build_tool_registry(
                    &AppConfig::default(),
                    &paths,
                    AgentMode::Normal,
                    false,
                )
                .unwrap(),
                AgentMode::Normal,
            )
            .unwrap();
            let prepared = agent.prepare_user_input("看图", &images).await.unwrap();
            format!("{:?}", prepared.hints)
        }
    };
    let prepare = |config: AppConfig, path: String| {
        let paths = paths.clone();
        async move {
            let state = StateStore::new(&paths).unwrap();
            let client =
                OpenAiCompatibleClient::new(config.provider(None).unwrap(), &config, &paths)
                    .unwrap();
            let agent = Agent::new(
                config,
                &paths,
                state,
                client,
                crate::tools::build_tool_registry(
                    &AppConfig::default(),
                    &paths,
                    AgentMode::Normal,
                    false,
                )
                .unwrap(),
                AgentMode::Normal,
            )
            .unwrap();
            let images = vec![Some(PastedImage::Path(path))];
            let prepared = agent.prepare_user_input("看图", &images).await.unwrap();
            format!("{:?}", prepared.hints)
        }
    };

    // 多模态:图已内联,不该出现工具邀请。
    let hints = prepare(
        build(vec!["text".into(), "image".into()]),
        picture_path.clone(),
    )
    .await;
    assert!(
        !hints.contains("vision_analyze"),
        "已内联的图不该再邀请调工具，实际提示：{hints}"
    );

    // 非多模态:图进不去,兜底提示必须还在。
    let hints = prepare(build(vec!["text".into()]), picture_path).await;
    assert!(
        hints.contains("vision_analyze"),
        "看不了图的模型必须保留工具兜底，实际提示：{hints}"
    );

    // 二进制(粘贴)图片走的是另一条提示分支,同样要钉住:临时文件路径保留
    // (生图参考等工具要用),工具邀请撤掉。
    let binary = || {
        vec![Some(PastedImage::Binary(ClipboardImage::new(
            "image/png".to_string(),
            vec![0x89, b'P', b'N', b'G'],
        )))]
    };
    let hints = prepare_binary(build(vec!["text".into(), "image".into()]), binary()).await;
    assert!(
        !hints.contains("vision_analyze"),
        "已内联的粘贴图不该再邀请调工具，实际提示：{hints}"
    );
    assert!(
        hints.contains("已保存到临时文件"),
        "临时文件路径要留着给别的工具用，实际提示：{hints}"
    );
    let hints = prepare_binary(build(vec!["text".into()]), binary()).await;
    assert!(
        hints.contains("vision_analyze"),
        "看不了图的模型必须保留工具兜底，实际提示：{hints}"
    );
}

#[test]
fn inline_media_message_uses_plain_for_text_and_parts_for_media() {
    use crate::state::{TurnInlineMedia, INLINE_MEDIA_KIND_IMAGE, INLINE_MEDIA_KIND_TEXT};
    let text = TurnInlineMedia {
        call_id: "c".into(),
        seq: 0,
        kind: INLINE_MEDIA_KIND_TEXT.into(),
        mime: "text/plain".into(),
        source: String::new(),
        data: Some(b"a cat".to_vec()),
    };
    let message = inline_media_message(&[text.clone()]).unwrap();
    assert!(matches!(message.content, Some(crate::llm::ChatContent::Text(ref t)) if t == "a cat"));

    let remote = TurnInlineMedia {
        call_id: "c".into(),
        seq: 1,
        kind: INLINE_MEDIA_KIND_IMAGE.into(),
        mime: String::new(),
        source: "https://example.com/p.jpg".into(),
        data: None,
    };
    let message = inline_media_message(&[text, remote]).unwrap();
    let parts = match message.content.unwrap() {
        crate::llm::ChatContent::Parts(parts) => parts,
        _ => panic!("parts"),
    };
    assert_eq!(parts.len(), 2);
    assert!(
        matches!(&parts[1], crate::llm::ChatContentPart::ImageUrl { image_url } if image_url.url == "https://example.com/p.jpg")
    );
    assert!(inline_media_message(&[]).is_none());
}
