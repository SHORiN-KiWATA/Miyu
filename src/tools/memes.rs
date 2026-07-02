use super::{vision, ToolRegistry, ToolSpec};
use crate::config::{AppConfig, MemesPluginConfig};
use crate::i18n::text as t;
use crate::llm::{ChatMessage, OpenAiCompatibleClient};
use crate::paths::MiyuPaths;
use crate::prompts::MEME_DESCRIPTION_PROMPT;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const BUILTIN_MEMES_DIR: &str = "/usr/share/miyu/memes";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MemeIndex {
    #[serde(default)]
    library: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    memes: Vec<MemeItem>,
    #[serde(default)]
    disabled_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemeItem {
    id: String,
    name: LocalizedName,
    file: String,
    mime_type: String,
    #[serde(default)]
    animated: bool,
    description: String,
    usage: String,
    avoid: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalizedName {
    #[serde(default)]
    zh: String,
    #[serde(default)]
    en: String,
}

#[derive(Debug, Clone)]
struct LoadedMeme {
    item: MemeItem,
    path: PathBuf,
    source: MemeSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutoMemeEvent {
    pub library: String,
    pub id: String,
    pub name: Value,
    pub description: String,
    pub usage: String,
    pub reason: String,
    pub sent_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AutoMemePlan {
    pub event: AutoMemeEvent,
    pub reminder: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AutoMemeState {
    #[serde(default)]
    last: Option<AutoMemeEvent>,
}

#[derive(Debug, Deserialize)]
struct AutoSendDecision {
    #[serde(default)]
    send: bool,
    #[serde(default)]
    id: String,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MemeSource {
    Builtin,
    User,
}

pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    if !config.plugins.memes.enabled {
        return;
    }
    registry.register(ToolSpec::new(
        "search_meme",
        t(
            "Search the current persona's meme library by scene, mood, tags, or visible content. Use before showing a meme unless the user provided a specific meme id.",
            "按场景、情绪、标签或画面内容搜索当前人格表情库。除非用户给了具体表情 id，否则发表情前先调用。",
        ),
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": t("Scene, mood, visible content, or user intent.", "场景、情绪、画面内容或用户意图。") },
                "tags": { "type": "array", "items": { "type": "string" }, "description": t("Optional preferred tags.", "可选偏好标签。") },
                "library": { "type": "string", "description": t("Optional meme library override.", "可选表情库覆盖。") },
                "limit": { "type": "integer", "description": t("Maximum number of candidates, default 6.", "候选数量上限，默认 6。") }
            },
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { search_meme(args, &config, &paths).await }
            }
        },
    ));
    registry.register(ToolSpec::new(
        "show_meme",
        t(
            "Render a meme in the terminal with chafa. GIFs are shown as static previews unless animation is explicitly allowed in config.",
            "发送表情包并使用 chafa 在终端渲染。GIF 默认显示静态预览，除非配置显式允许动画。",
        ),
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": t("Meme sha256 id.", "表情 sha256 id。") },
                "library": { "type": "string", "description": t("Optional meme library override.", "可选表情库覆盖。") },
                "size": { "type": "string", "description": t("Optional chafa size, e.g. 40x15.", "可选 chafa 尺寸，例如 40x15。") },
                "width": { "type": "integer", "description": t("Optional output width in terminal cells.", "可选终端单元格输出宽度。") },
                "height": { "type": "integer", "description": t("Optional output height in terminal cells.", "可选终端单元格输出高度。") }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { show_meme(args, &config, &paths).await }
            }
        },
    ));
    registry.register(ToolSpec::new(
        "recent_meme",
        t(
            "Get the most recent meme automatically sent for the current persona/library.",
            "查询当前人格/表情库最近一次自动发送的表情。",
        ),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |_| {
                let config = config.clone();
                let paths = paths.clone();
                async move { recent_meme(&config, &paths).await }
            }
        },
    ));
    registry.register(
        ToolSpec::new(
            "add_meme",
            t(
                "Add a local image to the current persona's writable meme library. If metadata is not supplied, the tool asks the configured vision model to generate it from the image.",
                "把本地图片加入当前人格的可写表情库。若未提供元数据，工具会调用配置的识图模型根据图片生成。",
            ),
            json!({
                "type": "object",
                "properties": {
                    "image": { "type": "string", "description": t("Local image path.", "本地图片路径。") },
                    "library": { "type": "string", "description": t("Optional meme library override.", "可选表情库覆盖。") },
                    "name_zh": { "type": "string", "description": t("Chinese display name.", "中文显示名。") },
                    "name_en": { "type": "string", "description": t("English display name.", "英文显示名。") },
                    "description": { "type": "string", "description": t("Visible content description.", "图片可见内容描述。") },
                    "usage": { "type": "string", "description": t("When to use this meme.", "什么时候使用该表情。") },
                    "avoid": { "type": "string", "description": t("When not to use this meme.", "什么场景不要使用。") },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": t("Search tags.", "检索标签。") }
                },
                "required": ["image"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move { add_meme(args, &config, &paths).await }
                }
            },
        )
        .writes(),
    );
    registry.register(
        ToolSpec::new(
            "update_meme",
            t(
                "Update meme index metadata in the writable overlay for the current library.",
                "更新当前表情库可写覆盖层中的表情元数据。",
            ),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": t("Meme sha256 id.", "表情 sha256 id。") },
                    "library": { "type": "string", "description": t("Optional meme library override.", "可选表情库覆盖。") },
                    "name_zh": { "type": "string" },
                    "name_en": { "type": "string" },
                    "description": { "type": "string" },
                    "usage": { "type": "string" },
                    "avoid": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "enabled": { "type": "boolean", "description": t("Enable or disable this meme.", "启用或禁用该表情。") }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move { update_meme(args, &config, &paths).await }
                }
            },
        )
        .writes(),
    );
    registry.register(
        ToolSpec::new(
            "delete_meme",
            t(
                "Delete a user meme or disable a built-in meme in the current library.",
                "删除用户表情，或在当前表情库中禁用内置表情。",
            ),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": t("Meme sha256 id.", "表情 sha256 id。") },
                    "library": { "type": "string", "description": t("Optional meme library override.", "可选表情库覆盖。") },
                    "hard_delete": { "type": "boolean", "description": t("Permanently remove user image instead of moving it to trash.", "永久删除用户图片，而不是移入回收站。") }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move { delete_meme(args, &config, &paths).await }
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
        .unwrap_or(6)
        .clamp(1, 20) as usize;
    let mut scored = load_library(paths, &library)?
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
        .map(|(score, meme)| {
            json!({
                "id": meme.item.id,
                "name": meme.item.name,
                "score": (score * 100.0).round() / 100.0,
                "description": meme.item.description,
                "usage": meme.item.usage,
                "avoid": meme.item.avoid,
                "tags": meme.item.tags,
                "animated": meme.item.animated,
                "source": source_label(meme.source),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "success": true, "library": library, "results": results }).to_string())
}

async fn show_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let id = required_str(&args, "id")?;
    let meme = find_meme(paths, &library, id)?.with_context(|| format!("meme not found: {id}"))?;
    let size = meme_print_size(&args, &config.plugins.memes);
    vision::print_image_file(&meme.path, size).await?;
    Ok(json!({
        "success": true,
        "library": library,
        "id": meme.item.id,
        "name": meme.item.name,
        "description": meme.item.description,
        "animated": meme.item.animated,
        "animation_note": if meme.item.animated && !config.plugins.memes.allow_gif_animation { Some("GIF was rendered as a normal chafa preview; animation is disabled by default.") } else { None },
    })
    .to_string())
}

async fn recent_meme(config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let state = load_auto_meme_state(config, paths)?;
    Ok(match state.last {
        Some(event) => json!({ "success": true, "recent": event }).to_string(),
        None => json!({ "success": false, "message": "当前人格/表情库还没有自动发送过表情" })
            .to_string(),
    })
}

pub(crate) async fn plan_auto_meme_before_reply(
    config: &AppConfig,
    paths: &MiyuPaths,
    client: &OpenAiCompatibleClient,
    user_message: &str,
) -> Result<Option<AutoMemePlan>> {
    let meme_config = &config.plugins.memes;
    if !meme_config.enabled
        || !meme_config.auto_send_enabled
        || user_message.trim().is_empty()
        || meme_config.auto_send_probability <= 0.0
    {
        return Ok(None);
    }
    if rand::random::<f32>() > meme_config.auto_send_probability.clamp(0.0, 1.0) {
        return Ok(None);
    }
    let library = meme_config.library_for_persona(&config.prompt.active_persona);
    let mut candidates = rank_memes(paths, &library, user_message, &[], 12)?;
    if candidates.is_empty() {
        candidates = rank_memes(paths, &library, "", &[], 12)?;
    }
    if candidates.is_empty() {
        return Ok(None);
    }
    let decision = decide_auto_send(client, user_message, &candidates).await?;
    let Some(decision) = decision else {
        return Ok(None);
    };
    if !decision.send || decision.confidence < meme_config.auto_send_min_confidence.clamp(0.0, 1.0)
    {
        return Ok(None);
    }
    let Some((_, meme)) = candidates
        .drain(..)
        .find(|(_, meme)| ids_match(&meme.item.id, &decision.id))
    else {
        return Ok(None);
    };
    let event = AutoMemeEvent {
        library,
        id: meme.item.id,
        name: serde_json::to_value(&meme.item.name)?,
        description: meme.item.description,
        usage: meme.item.usage,
        reason: decision.reason,
        sent_at: Utc::now().to_rfc3339(),
    };
    let reminder = format!(
        "<system-reminder>\n本轮回复发送后，程序会自动发送一张表情包。你在回复文字时应该自然地知道这件事，让语气和表情一致，但不要直白说“我将发送表情包”。\n计划发送表情：{}\n表情描述：{}\n适用场景：{}\n选择原因：{}\n</system-reminder>",
        display_name(&event.name),
        event.description,
        event.usage,
        event.reason,
    );
    Ok(Some(AutoMemePlan { event, reminder }))
}

pub(crate) async fn render_auto_meme(
    config: &AppConfig,
    paths: &MiyuPaths,
    event: &AutoMemeEvent,
) -> Result<()> {
    let meme = find_meme(paths, &event.library, &event.id)?
        .with_context(|| format!("meme not found: {}", event.id))?;
    vision::print_image_file(&meme.path, configured_meme_size(&config.plugins.memes)).await
}

pub(crate) fn record_auto_meme_event(
    config: &AppConfig,
    paths: &MiyuPaths,
    event: &AutoMemeEvent,
) -> Result<()> {
    let state = AutoMemeState {
        last: Some(event.clone()),
    };
    let path = auto_meme_state_path(config, paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&state)?))?;
    Ok(())
}

pub(crate) fn last_auto_meme_reminder(
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<Option<String>> {
    let Some(event) = load_auto_meme_state(config, paths)?.last else {
        return Ok(None);
    };
    Ok(Some(format!(
        "<system-reminder>\n上一轮回复文字发送后，程序自动补发了一张表情包。你需要自然地记得自己已经发过这张表情；如果用户提到“刚才那张/你发的表情”，按这个信息回答，不要说不知道。\n表情库：{}\n表情名：{}\n表情描述：{}\n适用场景：{}\n发送原因：{}\n发送时间：{}\n</system-reminder>",
        event.library,
        display_name(&event.name),
        event.description,
        event.usage,
        event.reason,
        event.sent_at,
    )))
}

async fn add_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let source = expand_path(required_str(&args, "image")?);
    let metadata = std::fs::metadata(&source)
        .with_context(|| format!("failed to stat image {}", source.display()))?;
    if !metadata.is_file() {
        bail!("image path is not a file: {}", source.display())
    }
    let max_bytes = config
        .plugins
        .memes
        .max_image_mb
        .saturating_mul(1024 * 1024);
    if metadata.len() > max_bytes {
        bail!(
            "image too large: {} bytes; limit is {} MiB",
            metadata.len(),
            config.plugins.memes.max_image_mb
        )
    }
    let bytes = std::fs::read(&source)
        .with_context(|| format!("failed to read image {}", source.display()))?;
    let digest = Sha256::digest(&bytes);
    let hash = format!("{digest:x}");
    let id = format!("sha256:{hash}");
    if let Some(existing) = find_meme(paths, &library, &id)? {
        return Ok(json!({
            "success": true,
            "already_exists": true,
            "library": library,
            "id": id,
            "name": existing.item.name,
            "path": existing.path,
        })
        .to_string());
    }
    let ext = image_ext(&source)?;
    let mime_type = mime_from_ext(ext)?;
    let animated = ext == "gif";
    let user_dir = user_library_dir(paths, &library);
    let images_dir = user_dir.join("images");
    std::fs::create_dir_all(&images_dir)?;
    let target_file = format!("{}.{}", &hash[..16], ext);
    let target = images_dir.join(&target_file);
    std::fs::copy(&source, &target).with_context(|| {
        format!(
            "failed to copy image {} to {}",
            source.display(),
            target.display()
        )
    })?;
    let mut item = if has_supplied_metadata(&args) {
        item_from_args(
            &args,
            id.clone(),
            format!("images/{target_file}"),
            mime_type,
            animated,
        )?
    } else {
        match describe_meme_image(config, paths, &source).await {
            Ok(metadata) => item_from_metadata(
                id.clone(),
                format!("images/{target_file}"),
                mime_type,
                animated,
                metadata,
            )?,
            Err(err) => {
                let _ = std::fs::remove_file(&target);
                return Ok(json!({
                    "success": false,
                    "needs_user_info": true,
                    "message": "vision metadata generation failed; ask the user what the image shows and when to use it, then call add_meme again with metadata fields",
                    "error": err.to_string(),
                })
                .to_string());
            }
        }
    };
    item.file = format!("images/{target_file}");
    let mut index = load_index(&user_dir.join("index.json"))?.unwrap_or_else(|| MemeIndex {
        library: library.clone(),
        version: 2,
        memes: Vec::new(),
        disabled_ids: Vec::new(),
    });
    index.library = library.clone();
    index.version = 2;
    index.disabled_ids.retain(|value| !ids_match(value, &id));
    index.memes.retain(|meme| !ids_match(&meme.id, &id));
    index.memes.push(item.clone());
    save_index(&user_dir.join("index.json"), &index)?;
    Ok(json!({
        "success": true,
        "library": library,
        "id": item.id,
        "name": item.name,
        "path": target,
        "metadata": item,
    })
    .to_string())
}

async fn update_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let id = required_str(&args, "id")?;
    let existing =
        find_meme(paths, &library, id)?.with_context(|| format!("meme not found: {id}"))?;
    let id = existing.item.id.clone();
    let user_dir = user_library_dir(paths, &library);
    let mut index = load_index(&user_dir.join("index.json"))?.unwrap_or_else(|| MemeIndex {
        library: library.clone(),
        version: 2,
        memes: Vec::new(),
        disabled_ids: Vec::new(),
    });
    index.library = library.clone();
    index.version = 2;
    let mut item = existing.item;
    apply_updates(&mut item, &args);
    if !index.memes.iter().any(|meme| ids_match(&meme.id, &id)) {
        index.memes.push(item.clone());
    } else {
        for meme in &mut index.memes {
            if ids_match(&meme.id, &id) {
                *meme = item.clone();
                break;
            }
        }
    }
    if let Some(enabled) = args.get("enabled").and_then(Value::as_bool) {
        if enabled {
            index.disabled_ids.retain(|value| !ids_match(value, &id));
        } else if !index.disabled_ids.iter().any(|value| ids_match(value, &id)) {
            index.disabled_ids.push(id.clone());
        }
    }
    save_index(&user_dir.join("index.json"), &index)?;
    Ok(json!({ "success": true, "library": library, "id": id, "metadata": item }).to_string())
}

async fn delete_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let requested_id = required_str(&args, "id")?;
    let user_dir = user_library_dir(paths, &library);
    let index_path = user_dir.join("index.json");
    let mut index = load_index(&index_path)?.unwrap_or_else(|| MemeIndex {
        library: library.clone(),
        version: 2,
        memes: Vec::new(),
        disabled_ids: Vec::new(),
    });
    index.library = library.clone();
    index.version = 2;
    if let Some(pos) = index
        .memes
        .iter()
        .position(|meme| ids_match(&meme.id, requested_id))
    {
        let item = index.memes.remove(pos);
        let id = item.id.clone();
        let path = user_dir.join(&item.file);
        if path.is_file() {
            if args
                .get("hard_delete")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                std::fs::remove_file(&path)?;
            } else {
                trash::delete(&path)?;
            }
        }
        index.disabled_ids.retain(|value| !ids_match(value, &id));
        save_index(&index_path, &index)?;
        return Ok(
            json!({ "success": true, "library": library, "id": id, "action": "deleted_user_meme" })
                .to_string(),
        );
    }
    if let Some(meme) = find_meme(paths, &library, requested_id)? {
        let id = meme.item.id;
        if !index.disabled_ids.iter().any(|value| ids_match(value, &id)) {
            index.disabled_ids.push(id.clone());
        }
        save_index(&index_path, &index)?;
        return Ok(json!({ "success": true, "library": library, "id": id, "action": "disabled_builtin_meme" }).to_string());
    }
    bail!("meme not found: {requested_id}")
}

async fn describe_meme_image(config: &AppConfig, paths: &MiyuPaths, image: &Path) -> Result<Value> {
    let text =
        vision::analyze_local_image_with_prompt(config, paths, image, MEME_DESCRIPTION_PROMPT)
            .await?;
    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map(|index| index + 1).unwrap_or(text.len());
    Ok(serde_json::from_str(&text[start..end])?)
}

async fn decide_auto_send(
    client: &OpenAiCompatibleClient,
    user_message: &str,
    candidates: &[(f32, LoadedMeme)],
) -> Result<Option<AutoSendDecision>> {
    let catalog = candidates
        .iter()
        .map(|(score, meme)| {
            json!({
                "id": meme.item.id,
                "local_score": (score * 100.0).round() / 100.0,
                "name": meme.item.name,
                "description": meme.item.description,
                "usage": meme.item.usage,
                "avoid": meme.item.avoid,
                "tags": meme.item.tags,
            })
        })
        .collect::<Vec<_>>();
    let prompt = format!(
        "你在 Miyu 回复前决定本轮是否应该搭配一张表情包。概率只控制触发频率；这里需要判断候选表情和用户消息、上下文语气的相关程度。请根据用户消息的语气、场景、关系边界和候选表情的 usage/avoid 决定。严肃、道歉、群管理、技术排障、长篇解释、用户明显在求助时不要发表情。轻松闲聊、调侃、打招呼、夸奖、吐槽、玩梗、情绪回应时可以发。只能从候选表情里选。confidence 表示所选表情与本轮消息/上下文的相关程度，0.0 到 1.0。只返回严格 JSON：{{\"send\": false, \"id\": \"\", \"confidence\": 0.0, \"reason\": \"\"}}\n\n用户消息：{}\n\n候选表情：{}",
        user_message.chars().take(1000).collect::<String>(),
        serde_json::to_string(&catalog)?,
    );
    let result = client
        .chat_stream(
            vec![
                ChatMessage::system("你是表情包发送决策器，只输出 JSON，不输出解释。"),
                ChatMessage::plain("user", prompt),
            ],
            Vec::new(),
            |_| Ok(()),
        )
        .await?;
    let Some(json_text) = json_slice(&result.content) else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<AutoSendDecision>(json_text).ok())
}

fn rank_memes(
    paths: &MiyuPaths,
    library: &str,
    query: &str,
    tags: &[String],
    limit: usize,
) -> Result<Vec<(f32, LoadedMeme)>> {
    let mut scored = load_library(paths, library)?
        .into_iter()
        .filter_map(|meme| {
            let score = score_meme(&meme.item, query, tags);
            (score > 0.0).then_some((score, meme))
        })
        .collect::<Vec<_>>();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit.max(1));
    Ok(scored)
}

fn load_library(paths: &MiyuPaths, library: &str) -> Result<Vec<LoadedMeme>> {
    let builtin_dir = builtin_library_dir(library);
    let user_dir = user_library_dir(paths, library);
    let builtin = load_index(&builtin_dir.join("index.json"))?.unwrap_or_default();
    let user = load_index(&user_dir.join("index.json"))?.unwrap_or_default();
    let disabled = user.disabled_ids;
    let mut user_ids = Vec::new();
    let mut result = Vec::new();
    for item in user.memes {
        if disabled.iter().any(|id| ids_match(id, &item.id)) {
            continue;
        }
        user_ids.push(item.id.clone());
        result.push(LoadedMeme {
            path: user_dir.join(&item.file),
            item,
            source: MemeSource::User,
        });
    }
    for item in builtin.memes {
        if disabled.iter().any(|id| ids_match(id, &item.id))
            || user_ids.iter().any(|id| ids_match(id, &item.id))
        {
            continue;
        }
        result.push(LoadedMeme {
            path: builtin_dir.join(&item.file),
            item,
            source: MemeSource::Builtin,
        });
    }
    Ok(result)
}

fn find_meme(paths: &MiyuPaths, library: &str, id: &str) -> Result<Option<LoadedMeme>> {
    Ok(load_library(paths, library)?
        .into_iter()
        .find(|meme| ids_match(&meme.item.id, id)))
}

fn ids_match(stored: &str, requested: &str) -> bool {
    let stored = id_hash_part(stored);
    let requested = id_hash_part(requested);
    !requested.is_empty() && stored.starts_with(requested)
}

fn id_hash_part(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix("sha256:").unwrap_or(value)
}

fn load_index(path: &Path) -> Result<Option<MemeIndex>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&std::fs::read_to_string(path)?)?))
}

fn save_index(path: &Path, index: &MemeIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(index)?)?;
    Ok(())
}

fn selected_library(args: &Value, config: &AppConfig) -> String {
    args.get("library")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(sanitize_library)
        .unwrap_or_else(|| {
            config
                .plugins
                .memes
                .library_for_persona(&config.prompt.active_persona)
        })
}

fn load_auto_meme_state(config: &AppConfig, paths: &MiyuPaths) -> Result<AutoMemeState> {
    let path = auto_meme_state_path(config, paths);
    if !path.is_file() {
        return Ok(AutoMemeState::default());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

fn auto_meme_state_path(config: &AppConfig, paths: &MiyuPaths) -> PathBuf {
    let library = config
        .plugins
        .memes
        .library_for_persona(&config.prompt.active_persona);
    paths
        .state_dir
        .join("memes")
        .join(sanitize_library(&library))
        .join("auto-send.json")
}

fn display_name(name: &Value) -> String {
    let zh = name.get("zh").and_then(Value::as_str).unwrap_or_default();
    let en = name.get("en").and_then(Value::as_str).unwrap_or_default();
    if !zh.trim().is_empty() {
        zh.to_string()
    } else if !en.trim().is_empty() {
        en.to_string()
    } else {
        "未命名表情".to_string()
    }
}

fn json_slice(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end >= start).then_some(&text[start..=end])
}

fn sanitize_library(value: &str) -> String {
    let value = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if value.is_empty() {
        "default".to_string()
    } else {
        value
    }
}

fn builtin_library_dir(library: &str) -> PathBuf {
    if let Some(path) = std::env::var_os("MIYU_MEMES_DIR") {
        return PathBuf::from(path).join(library);
    }
    let dev = PathBuf::from("src/memes").join(library);
    if dev.is_dir() {
        return dev;
    }
    PathBuf::from(BUILTIN_MEMES_DIR).join(library)
}

fn user_library_dir(paths: &MiyuPaths, library: &str) -> PathBuf {
    paths.data_dir.join("memes").join(sanitize_library(library))
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("{key} is required")
    }
    Ok(value)
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn score_meme(item: &MemeItem, query: &str, tags: &[String]) -> f32 {
    let query = normalize(&format!("{query} {}", tags.join(" ")));
    if query.is_empty() {
        return 0.1;
    }
    let haystack = normalize(&format!(
        "{} {} {} {} {} {}",
        item.name.zh,
        item.name.en,
        item.description,
        item.usage,
        item.avoid,
        item.tags.join(" ")
    ));
    let mut score = 0.0;
    for term in query.split_whitespace() {
        if haystack.contains(term) {
            score += if item.tags.iter().any(|tag| normalize(tag).contains(term)) {
                2.0
            } else {
                1.0
            };
        }
    }
    if haystack.contains(&query) {
        score += 2.0;
    }
    score
}

fn normalize(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_punctuation() { ' ' } else { ch })
        .collect::<String>()
}

fn meme_print_size(args: &Value, config: &MemesPluginConfig) -> Option<String> {
    let width = args
        .get("width")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(160);
    let height = args
        .get("height")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(80);
    match (width, height) {
        (0, 0) => args
            .get("size")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| configured_meme_size(config)),
        (width, 0) => Some(format!("{width}x")),
        (0, height) => Some(format!("x{height}")),
        (width, height) => Some(format!("{width}x{height}")),
    }
}

fn configured_meme_size(config: &MemesPluginConfig) -> Option<String> {
    let (cols, rows) = crossterm::terminal::size().ok()?;
    let width = ((cols as u32 * config.width_percent as u32) / 100).clamp(1, 160);
    let height = ((rows as u32 * config.height_percent as u32) / 100).clamp(1, 80);
    Some(format!("{width}x{height}"))
}

fn expand_path(value: &str) -> PathBuf {
    if let Some(rest) = value.trim().strip_prefix("~/") {
        if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
            return home.join(rest);
        }
    }
    let path = Path::new(value.trim());
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn image_ext(path: &Path) -> Result<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Ok("jpg"),
        "png" => Ok("png"),
        "webp" => Ok("webp"),
        "gif" => Ok("gif"),
        value => {
            bail!("unsupported image extension: {value}; supported: jpg, jpeg, png, webp, gif")
        }
    }
}

fn mime_from_ext(ext: &str) -> Result<String> {
    Ok(match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        value => bail!("unsupported image extension: {value}"),
    }
    .to_string())
}

fn has_supplied_metadata(args: &Value) -> bool {
    [
        "name_zh",
        "name_en",
        "description",
        "usage",
        "avoid",
        "tags",
    ]
    .iter()
    .any(|key| args.get(*key).is_some())
}

fn item_from_args(
    args: &Value,
    id: String,
    file: String,
    mime_type: String,
    animated: bool,
) -> Result<MemeItem> {
    let name = LocalizedName {
        zh: args
            .get("name_zh")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        en: args
            .get("name_en")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
    };
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let usage = args
        .get("usage")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.zh.is_empty() || description.is_empty() || usage.is_empty() {
        bail!("name_zh, description, and usage are required when supplying metadata manually")
    }
    Ok(MemeItem {
        id,
        name,
        file,
        mime_type,
        animated,
        description,
        usage,
        avoid: args
            .get("avoid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        tags: string_array(args.get("tags")),
    })
}

fn item_from_metadata(
    id: String,
    file: String,
    mime_type: String,
    animated: bool,
    metadata: Value,
) -> Result<MemeItem> {
    let name = metadata.get("name").cloned().unwrap_or_default();
    let item = MemeItem {
        id,
        name: LocalizedName {
            zh: name
                .get("zh")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
            en: name
                .get("en")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        },
        file,
        mime_type,
        animated,
        description: metadata
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        usage: metadata
            .get("usage")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        avoid: metadata
            .get("avoid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string(),
        tags: string_array(metadata.get("tags")),
    };
    if item.name.zh.is_empty() || item.description.is_empty() || item.usage.is_empty() {
        bail!("vision metadata is incomplete")
    }
    Ok(item)
}

fn apply_updates(item: &mut MemeItem, args: &Value) {
    if let Some(value) = args
        .get("name_zh")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.name.zh = value.to_string();
    }
    if let Some(value) = args
        .get("name_en")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.name.en = value.to_string();
    }
    if let Some(value) = args
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.description = value.to_string();
    }
    if let Some(value) = args
        .get("usage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        item.usage = value.to_string();
    }
    if let Some(value) = args.get("avoid").and_then(Value::as_str).map(str::trim) {
        item.avoid = value.to_string();
    }
    if args.get("tags").is_some() {
        item.tags = string_array(args.get("tags"));
    }
}

fn source_label(source: MemeSource) -> &'static str {
    match source {
        MemeSource::Builtin => "builtin",
        MemeSource::User => "user",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_library_keeps_simple_names() {
        assert_eq!(sanitize_library("Miyu"), "miyu");
        assert_eq!(sanitize_library("默认 表情"), "default");
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
            avoid: String::new(),
            tags: vec!["Linux".to_string(), "企鹅".to_string()],
        };
        assert!(score_meme(&item, "Linux", &[]) > score_meme(&item, "炸鸡", &[]));
    }

    #[test]
    fn matches_full_prefixed_and_short_ids() {
        let id = "sha256:abcdef1234567890";
        assert!(ids_match(id, "sha256:abcdef1234567890"));
        assert!(ids_match(id, "abcdef1234567890"));
        assert!(ids_match(id, "abcdef12"));
        assert!(!ids_match(id, "123456"));
    }
}
