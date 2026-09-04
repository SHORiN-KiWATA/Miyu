mod crud;
mod dashboard;
mod library;
mod validate;
pub(crate) use crud::*;
pub(crate) use dashboard::*;
pub(crate) use library::*;
pub(crate) use validate::*;

use super::{vision, ToolRegistry, ToolSpec};
use crate::config::{AppConfig, MemesPluginConfig};
use crate::paths::MiyuPaths;
use crate::prompts::MEME_DESCRIPTION_PROMPT;
use anyhow::{bail, Context, Result};
use image::AnimationDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{BufReader, Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

static MEME_LIBRARY_CACHE: OnceLock<RwLock<Option<MemeLibraryCache>>> = OnceLock::new();
static MEME_LIBRARY_LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemeRef {
    pub(crate) library: String,
    pub(crate) id: String,
}

pub(crate) fn auto_meme_reminder(
    config: &AppConfig,
    user_message: &str,
    platform: bool,
) -> Option<String> {
    let meme_config = &config.plugins.memes;
    let auto_enabled = if platform {
        meme_config.auto_send_platform_enabled
    } else {
        meme_config.auto_send_enabled
    };
    if !meme_config.enabled
        || !auto_enabled
        || user_message.trim().is_empty()
        || meme_config.auto_send_probability <= 0.0
    {
        return None;
    }
    if rand::random::<f32>() > meme_config.auto_send_probability.clamp(0.0, 1.0) {
        return None;
    }
    Some(
        "<system-reminder>\n<send_meme_plan>\n触发自动发送表情包提醒。注意！本轮回复时你必须发送表情包。\n\n- 不要提及本提醒。\n- 根据上下文判断表情包是否合适，若匹配程度不足95%则不发送。\n- 不要说“我将发送表情包”。\n- 如果决定发送，应让文字回复和表情包语气自然一致。\n</send_meme_plan>\n</system-reminder>"
            .to_string(),
    )
}

/// 表情包分两个工具：读路径 `use_meme`，写路径 `manage_meme`。
///
/// 这么分不是按「像不像一件事」，是按**加载成本**：发表情是高频动作，
/// `use_meme` 必须常驻目录，否则每次（含 QQ 自动发送）都要先 `load_tools`；
/// 改表情库是低频动作，`manage_meme` 挂在 `memes` 组里懒加载，平时只占一条
/// 精简条目。
///
/// 两个上下文都注册这两个工具：群聊里也要能加表情。`manage_meme` 是写工具，
/// 但它只写人格自己的表情库目录，和 `generate_image` 同类——平台注册表那条
/// 「全员 ReadOnly」的断言把它们俩列为明示例外。
pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    if !config.plugins.memes.enabled {
        return;
    }
    register_use(registry, config.clone(), paths.clone());
    register_manage(registry, config, paths);
}

/// 平台/群聊上下文：读写都给。
pub fn register_chat(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    register(registry, config, paths);
}

fn register_use(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    // 参数与描述都来自 descriptions/use_meme.json（注册后被整体覆盖），
    // 这里给的只是占位。
    registry.register(ToolSpec::new_with_progress(
        "use_meme",
        "Search and send memes from the current persona's library.",
        json!({
            "type": "object",
            "properties": { "action": { "type": "string" } },
            "required": ["action"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args, progress| {
                let config = config.clone();
                let paths = paths.clone();
                async move {
                    match args
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "search" => search_meme(args, &config, &paths).await,
                        "show" => show_meme(args, &config, &paths, progress).await,
                        other => bail!("unknown action: {other}; expected search or show"),
                    }
                }
            }
        },
    ));
}

fn register_manage(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    registry.register(
        ToolSpec::new(
            "manage_meme",
            "Maintain the current persona's writable meme library.",
            json!({
                "type": "object",
                "properties": { "action": { "type": "string" } },
                "required": ["action"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move {
                        match args
                            .get("action")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                        {
                            "add" => add_meme(args, &config, &paths).await,
                            "update" => update_meme(args, &config, &paths).await,
                            "delete" => delete_meme(args, &config, &paths).await,
                            other => {
                                bail!("unknown action: {other}; expected add, update or delete")
                            }
                        }
                    }
                }
            },
        )
        .writes(),
    );
}

async fn search_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tags = string_array(args.get("tags"));
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(config.plugins.memes.search_max_results as u64)
        .clamp(1, 3) as usize;
    let loaded = load_library(paths, &library)?;
    let ids = meme_ids(&loaded);
    let mut scored = loaded
        .into_iter()
        .filter_map(|meme| {
            let score = score_meme(&meme.item, query, &tags);
            (score > 0.0).then_some((score, meme))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let results = scored
        .into_iter()
        .take(limit)
        // 返回体只留"挑哪张"真正要用的字段(08-17 实测:125 条记录 55,708
        // 字符里,origin 采集元数据占 20.2%、score 全精度浮点 5.4%、
        // source 4.3%、name.en 约 5%,对选表情包零价值)。tags 照旧参与
        // 上面的 score_meme 排序,只是不再发给模型。
        .map(|(_score, meme)| meme)
        .collect::<Vec<_>>();
    // 08-21 token-diet:候选列表改为行格式,不再逐条 JSON 重复键名。
    let mut output = format!("library {library}: {} candidate(s)\n", results.len());
    for meme in &results {
        let mut line = format!(
            "- id={} {}",
            unique_short_id_from_ids(&ids, &meme.item.id),
            meme.item.name.zh
        );
        if !meme.item.description.is_empty() {
            line.push_str(&format!(" — {}", meme.item.description));
        }
        if !meme.item.usage.is_empty() {
            line.push_str(&format!(" | usage: {}", meme.item.usage));
        }
        if meme.item.animated {
            line.push_str(" [animated]");
        }
        output.push_str(&line);
        output.push('\n');
    }
    Ok(output)
}

async fn show_meme(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
    progress: crate::tools::ToolProgress,
) -> Result<String> {
    let library = selected_library(&args, config);
    let id = required_str(&args, "id")?;
    let memes = load_library(paths, &library)?;
    let ids = meme_ids(&memes);
    let meme = find_meme_in(memes, id)?.with_context(|| format!("meme not found: {id}"))?;
    let size = meme_print_size(&args, &config.plugins.memes);
    progress.report_image(meme.path.clone(), meme.item.description.clone());
    if progress.prepare_for_external_output().await {
        if meme.item.animated {
            let preview = static_gif_preview(&meme.path).await?;
            vision::print_image_file(preview.path(), size).await?;
        } else {
            vision::print_image_file(&meme.path, size).await?;
        }
    }
    // 文本形态;compact_sent_meme_report(agent/reports.rs)对新旧两种形态
    // 都能解析。
    let short_id = unique_short_id_from_ids(&ids, &meme.item.id);
    if meme.item.description.is_empty() {
        Ok(format!("sent meme {short_id}"))
    } else {
        Ok(format!("sent meme {short_id}: {}", meme.item.description))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Delay, Frame, ImageEncoder, Rgba, RgbaImage};

    #[test]
    fn sanitize_library_keeps_simple_names() {
        assert_eq!(sanitize_library("Miyu"), "miyu");
        assert_eq!(sanitize_library("默认 表情"), "default");
    }

    /// 自动提示发送表情的平台/本地开关相互独立(两者默认都开)。
    #[test]
    fn platform_and_local_auto_send_gates_are_independent() {
        let mut config = AppConfig::default();
        config.plugins.memes.auto_send_probability = 1.0;
        assert!(auto_meme_reminder(&config, "你好", true).is_some());
        assert!(auto_meme_reminder(&config, "你好", false).is_some());
        config.plugins.memes.auto_send_enabled = false;
        assert!(auto_meme_reminder(&config, "你好", false).is_none());
        assert!(auto_meme_reminder(&config, "你好", true).is_some());
        config.plugins.memes.auto_send_enabled = true;
        config.plugins.memes.auto_send_platform_enabled = false;
        assert!(auto_meme_reminder(&config, "你好", false).is_some());
        assert!(auto_meme_reminder(&config, "你好", true).is_none());
    }

    #[test]
    fn scores_tag_matches_higher_than_no_match() {
        let item = MemeItem {
            id: "sha256:test".to_string(),
            name: LocalizedName {
                zh: "Linux 企鹅".to_string(),
                en: "Linux Penguin".to_string(),
            },
            file: "images/test.png".to_string(),
            mime_type: "image/png".to_string(),
            animated: false,
            description: "戴墨镜的企鹅抱着终端".to_string(),
            usage: "适合 Linux 话题".to_string(),
            tags: vec!["Linux".to_string(), "企鹅".to_string()],
            origin: None,
        };
        assert!(score_meme(&item, "Linux", &[]) > score_meme(&item, "炸鸡", &[]));
    }

    #[test]
    fn current_library_follows_persona_mapping() {
        let mut config = AppConfig::default();
        assert_eq!(current_persona_library(&config), "miyu");
        config.prompt.active_persona = "Custom Persona.md".to_string();
        config.plugins.memes.persona_libraries.insert(
            config.active_persona_scope(),
            "Shared Reactions".to_string(),
        );
        assert_eq!(current_persona_library(&config), "shared-reactions");
    }

    #[test]
    fn strict_classification_requires_all_acceptance_gates() {
        let accepted = accepted_classification();
        validate_classification(&accepted).unwrap();

        let mut low_confidence = accepted.clone();
        low_confidence.confidence = 99;
        assert!(validate_classification(&low_confidence).is_err());

        let mut missing_positive = accepted.clone();
        missing_positive.positive_gates.reusable = false;
        assert!(validate_classification(&missing_positive).is_err());

        let mut ordinary_photo = accepted;
        ordinary_photo.risk_gates.ordinary_photo = true;
        assert!(validate_classification(&ordinary_photo).is_err());
    }

    #[test]
    fn rejected_classification_never_becomes_an_item() {
        let mut rejected = accepted_classification();
        rejected.save = false;
        validate_classification(&rejected).unwrap();
        assert!(item_from_classification(
            "sha256:test".to_string(),
            "images/test.png".to_string(),
            "image/png".to_string(),
            false,
            rejected,
            None,
        )
        .is_err());
    }

    #[test]
    fn meme_item_origin_roundtrips_and_stays_backward_compatible() {
        let legacy = r#"{"id":"sha256:x","name":{"zh":"名","en":""},"file":"images/x.png","mime_type":"image/png","description":"d","usage":"u","avoid":""}"#;
        let item: MemeItem = serde_json::from_str(legacy).unwrap();
        assert!(item.origin.is_none());
        assert!(!serde_json::to_string(&item).unwrap().contains("origin"));

        let with_origin = MemeItem {
            origin: Some(MemeOrigin {
                platform: "onebot".to_string(),
                sender_id: "10001".to_string(),
                sender_name: "群友".to_string(),
                sent_at: "2026-08-10T12:00:00+00:00".to_string(),
                ..Default::default()
            }),
            ..item
        };
        let text = serde_json::to_string(&with_origin).unwrap();
        let back: MemeItem = serde_json::from_str(&text).unwrap();
        let origin = back.origin.unwrap();
        assert_eq!(origin.sender_id, "10001");
        assert_eq!(origin.sender_name, "群友");
        assert_eq!(origin.sent_at, "2026-08-10T12:00:00+00:00");
    }

    /// 真实链路实测：cargo test --bin miyu -- --ignored collect_meme_records_origin
    /// 需要 MIYU_E2E_CONFIG_DIR 指向含识图模型配置的真实 config 目录，
    /// MIYU_E2E_IMAGE 指向一张能通过表情判定的图片；数据写入临时目录。
    #[tokio::test]
    #[ignore = "hits the real vision model; needs MIYU_E2E_CONFIG_DIR + MIYU_E2E_IMAGE"]
    async fn collect_meme_records_origin_end_to_end() {
        let config_dir = PathBuf::from(std::env::var("MIYU_E2E_CONFIG_DIR").unwrap());
        let image = PathBuf::from(std::env::var("MIYU_E2E_IMAGE").unwrap());
        let temp = tempfile::tempdir().unwrap();
        let paths = MiyuPaths {
            root_dir: config_dir.clone(),
            config_dir: config_dir.clone(),
            config_file: config_dir.join("config.jsonc"),
            skills_dir: config_dir.join("skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: config_dir.join("scripts"),
            system_scripts_dir: PathBuf::new(),
        };
        let config = AppConfig::load_or_default(&paths).unwrap();
        let origin = MemeOrigin {
            platform: "onebot".to_string(),
            conversation_kind: "group".to_string(),
            conversation_id: "123456".to_string(),
            sender_id: "10001".to_string(),
            sender_name: "测试群友".to_string(),
            message_id: "msg-e2e-1".to_string(),
            sent_at: "2026-08-10T12:00:00+00:00".to_string(),
            collected_at: String::new(),
            reason: String::new(),
        };
        let outcome = collect_meme_from_local_image(&image, &config, &paths, Some(origin))
            .await
            .unwrap();
        let meme = match outcome {
            MemeCollectionOutcome::Accepted { meme } => meme,
            other => panic!("expected acceptance, got {other:?}"),
        };
        let index_path = user_library_dir(&paths, &meme.library).join("index.json");
        let index: MemeIndex =
            serde_json::from_str(&std::fs::read_to_string(index_path).unwrap()).unwrap();
        let saved = index
            .memes
            .iter()
            .find(|item| item.id == meme.id)
            .expect("saved meme in index");
        let origin = saved.origin.as_ref().expect("origin recorded");
        assert_eq!(origin.sender_id, "10001");
        assert_eq!(origin.sender_name, "测试群友");
        assert_eq!(origin.sent_at, "2026-08-10T12:00:00+00:00");
        assert!(!origin.collected_at.is_empty(), "collected_at stamped");
        println!(
            "E2E origin: {}",
            serde_json::to_string_pretty(origin).unwrap()
        );
    }

    #[test]
    fn strict_schema_rejects_unknown_and_missing_fields() {
        let mut value = serde_json::to_value(classification_json()).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<MemeClassification>(value).is_err());

        let mut missing = classification_json();
        missing.as_object_mut().unwrap().remove("confidence");
        assert!(serde_json::from_value::<MemeClassification>(missing).is_err());

        let mut nested = classification_json();
        nested["name"]["unexpected"] = json!("value");
        assert!(serde_json::from_value::<MemeClassification>(nested).is_err());
    }

    #[test]
    fn classification_enforces_metadata_and_tag_limits() {
        let mut classification = accepted_classification();
        classification.description = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert!(validate_classification(&classification).is_err());

        let mut duplicate_tags = accepted_classification();
        duplicate_tags.tags = vec!["Happy".to_string(), "happy".to_string()];
        assert!(validate_classification(&duplicate_tags).is_err());

        let mut spaced_tag = accepted_classification();
        spaced_tag.tags = vec!["not short".to_string()];
        assert!(validate_classification(&spaced_tag).is_err());
    }

    #[test]
    fn image_validation_uses_content_not_extension() {
        let bytes = png_bytes(64, 48);
        assert_eq!(
            validate_image_bytes(&bytes).unwrap(),
            ValidatedImageFormat::Png
        );
        assert!(validate_image_bytes(b"not an image").is_err());
    }

    #[test]
    fn image_validation_enforces_dimension_bounds() {
        assert!(validate_image_bytes(&png_bytes(31, 64)).is_err());
        assert!(validate_image_bytes(&png_bytes(64, 32)).is_ok());
        assert!(validate_dimensions(4096, 3907).is_err());
    }

    #[test]
    fn gif_validation_enforces_frame_and_duration_limits() {
        assert!(validate_image_bytes(&gif_bytes(2, 100)).is_ok());
        assert!(validate_image_bytes(&gif_bytes(2, 8_000)).is_err());
        assert!(validate_image_bytes(&gif_bytes(MAX_GIF_FRAMES + 1, 1)).is_err());
    }

    #[tokio::test]
    async fn gif_terminal_preview_is_a_static_png() {
        let mut source = tempfile::Builder::new().suffix(".gif").tempfile().unwrap();
        source.write_all(&gif_bytes(2, 100)).unwrap();
        let preview = static_gif_preview(source.path()).await.unwrap();
        let reader = image::ImageReader::open(preview.path())
            .unwrap()
            .with_guessed_format()
            .unwrap();
        assert_eq!(reader.format(), Some(image::ImageFormat::Png));
    }

    #[test]
    fn index_save_replaces_atomically_and_remains_parseable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("library/index.json");
        let mut index = MemeIndex {
            library: "test".to_string(),
            version: 2,
            memes: Vec::new(),
            disabled_ids: Vec::new(),
        };
        save_index(&path, &index).unwrap();
        index.disabled_ids.push("sha256:abc".to_string());
        save_index(&path, &index).unwrap();
        assert_eq!(
            load_index(&path).unwrap().unwrap().disabled_ids,
            index.disabled_ids
        );
    }

    #[test]
    fn matches_full_prefixed_and_short_ids() {
        let id = "sha256:abcdef1234567890";
        assert!(ids_match(id, "sha256:abcdef1234567890"));
        assert!(ids_match(id, "abcdef1234567890"));
        assert!(ids_match(id, "abcdef12"));
        assert!(!ids_match(id, "123456"));
    }

    #[test]
    fn unique_short_id_starts_at_git_style_length() {
        let ids = vec!["sha256:abcdef1234567890".to_string()];

        assert_eq!(
            unique_short_id_from_ids(&ids, "sha256:abcdef1234567890"),
            "abcdef1"
        );
    }

    #[test]
    fn unique_short_id_extends_until_unambiguous() {
        let ids = vec![
            "sha256:abcdef1234567890".to_string(),
            "sha256:abcdef1999999999".to_string(),
        ];

        assert_eq!(
            unique_short_id_from_ids(&ids, "sha256:abcdef1234567890"),
            "abcdef12"
        );
    }

    #[test]
    fn find_meme_rejects_too_short_prefix() {
        let err = find_meme_in(vec![test_loaded_meme("sha256:abcdef1234567890")], "abcdef")
            .unwrap_err()
            .to_string();

        assert!(err.contains("too short"));
    }

    #[test]
    fn find_meme_rejects_ambiguous_prefix() {
        let err = find_meme_in(
            vec![
                test_loaded_meme("sha256:abcdef1234567890"),
                test_loaded_meme("sha256:abcdef1999999999"),
            ],
            "abcdef1",
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("ambiguous"));
    }

    #[test]
    fn find_meme_accepts_unique_short_prefix() {
        let meme = find_meme_in(vec![test_loaded_meme("sha256:abcdef1234567890")], "abcdef1")
            .unwrap()
            .unwrap();

        assert_eq!(meme.item.id, "sha256:abcdef1234567890");
    }

    fn test_loaded_meme(id: &str) -> LoadedMeme {
        LoadedMeme {
            item: MemeItem {
                id: id.to_string(),
                name: LocalizedName {
                    zh: "测试".to_string(),
                    en: "test".to_string(),
                },
                file: "images/test.png".to_string(),
                mime_type: "image/png".to_string(),
                animated: false,
                description: "测试表情".to_string(),
                usage: "测试".to_string(),
                tags: Vec::new(),
                origin: None,
            },
            path: PathBuf::from("images/test.png"),
            source: MemeSource::User,
        }
    }

    fn accepted_classification() -> MemeClassification {
        MemeClassification {
            save: true,
            // 兼容字段:模型仍可能吐 avoid,收下即丢(见 crud.rs 的说明)
            avoid: String::new(),
            reason: String::new(),
            confidence: 100,
            positive_gates: PositiveGates {
                chat_reaction: true,
                emotion_or_meme: true,
                reusable: true,
                context_independent: true,
                persona_fit: true,
                meaning_clear: true,
                visual_quality: true,
            },
            risk_gates: RiskGates {
                ordinary_photo: false,
                informational_content: false,
                privacy: false,
                advertisement: false,
                unsafe_or_abusive: false,
            },
            name: LocalizedName {
                zh: "开心猫".to_string(),
                en: "Happy Cat".to_string(),
            },
            description: "一只卡通猫开心地挥手。".to_string(),
            usage: "适合轻松打招呼。".to_string(),
            tags: vec!["开心".to_string(), "猫".to_string()],
        }
    }

    fn classification_json() -> Value {
        json!({
            "save": true,
            "confidence": 100,
            "positive_gates": {
                "chat_reaction": true,
                "emotion_or_meme": true,
                "reusable": true,
                "context_independent": true,
                "persona_fit": true,
                "meaning_clear": true,
                "visual_quality": true
            },
            "risk_gates": {
                "ordinary_photo": false,
                "informational_content": false,
                "privacy": false,
                "advertisement": false,
                "unsafe_or_abusive": false
            },
            "name": { "zh": "开心猫", "en": "Happy Cat" },
            "description": "一只卡通猫开心地挥手。",
            "usage": "适合轻松打招呼。",
            "avoid": "严肃场景不要使用。",
            "tags": ["开心", "猫"]
        })
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, Rgba([20, 40, 60, 255]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(
                image.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        bytes
    }

    fn gif_bytes(frames: usize, delay_ms: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            let frames = (0..frames).map(|_| {
                Frame::from_parts(
                    RgbaImage::from_pixel(32, 32, Rgba([20, 40, 60, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(delay_ms, 1),
                )
            });
            encoder.encode_frames(frames).unwrap();
        }
        bytes
    }
}

#[cfg(test)]
mod register_tests {
    use super::*;
    use crate::paths::MiyuPaths;

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: std::path::PathBuf::new(),
        }
    }

    fn actions_of(registry: &ToolRegistry, tool: &str) -> Vec<String> {
        let definition = registry
            .definitions()
            .into_iter()
            .find(|definition| definition.function.name == tool)
            .unwrap_or_else(|| panic!("{tool} 应当注册"));
        definition.function.parameters["properties"]["action"]["enum"]
            .as_array()
            .expect("action 应当是枚举")
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// 读写分成两个工具，各自只认自己那几个动作。
    #[test]
    fn the_read_and_write_paths_are_separate_tools() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut registry = ToolRegistry::new();
        register(&mut registry, AppConfig::default(), paths);
        assert_eq!(actions_of(&registry, "use_meme"), ["search", "show"]);
        assert_eq!(
            actions_of(&registry, "manage_meme"),
            ["add", "update", "delete"]
        );
        assert_eq!(
            registry.permission("use_meme").unwrap(),
            crate::tools::ToolPermission::ReadOnly
        );
        assert_eq!(
            registry.permission("manage_meme").unwrap(),
            crate::tools::ToolPermission::Writes
        );
    }

    /// 群聊拿到的和终端一样——加表情在群里也要能用。
    #[test]
    fn the_chat_registry_gets_both_tools() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut registry = ToolRegistry::new();
        register_chat(&mut registry, AppConfig::default(), paths);
        assert!(registry.contains("use_meme"));
        assert!(registry.contains("manage_meme"));
    }

    /// 发表情是高频动作，`use_meme` 必须常驻目录，否则每次都要先 load_tools；
    /// 改表情库是低频动作，`manage_meme` 挂在 memes 组里懒加载。
    #[test]
    fn only_the_read_path_stays_in_the_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut registry = ToolRegistry::new();
        register(&mut registry, AppConfig::default(), paths);
        let loaded = std::collections::BTreeSet::new();
        let names = registry
            .lazy_definitions(&loaded)
            .into_iter()
            .map(|definition| definition.function.name)
            .collect::<Vec<_>>();
        assert!(names.iter().any(|name| name == "use_meme"));
        assert!(!names.iter().any(|name| name == "manage_meme"));
    }

    /// 动作写错要报错，不能静默什么都不做。
    #[tokio::test]
    async fn an_unknown_action_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let mut registry = ToolRegistry::new();
        register(&mut registry, AppConfig::default(), paths);
        let error = registry
            .call("use_meme", &json!({"action": "delete"}).to_string())
            .await
            .expect_err("use_meme 不该认 delete");
        assert!(
            error.to_string().contains("search or show"),
            "实际：{error}"
        );
    }
}
