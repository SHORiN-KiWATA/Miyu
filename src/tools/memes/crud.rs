//! 表情的增删改与分类。
//!
//! `add_meme` 最长，因为它要一路做完：下载/读入、校验、让模型分类、写库、
//! 落盘。中途任一步失败都要把已写的部分清掉，否则库里会留下指向不存在文件的
//! 条目。

use crate::tools::memes::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum MemeCollectionOutcome {
    Accepted { meme: MemeRef },
    Rejected { reason: String },
    AlreadyExists { meme: MemeRef },
}

/// 这几个结构体刻意不加 `deny_unknown_fields`:多出来的字段既不会让判定变错,
/// 却会让整次入库炸在解析上(`avoid` 就是这么撞出来的)。缺字段照样会被 serde
/// 挡下,真正的把关在 validate_classification。
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MemeClassification {
    pub(crate) save: bool,
    pub(crate) confidence: u8,
    pub(crate) positive_gates: PositiveGates,
    pub(crate) risk_gates: RiskGates,
    pub(crate) name: LocalizedName,
    pub(crate) description: String,
    pub(crate) usage: String,
    /// 已废弃, 提示词早就不要它了, 但描述模型仍会习惯性吐出来。收下即丢。
    #[serde(default)]
    pub(crate) avoid: String,
    pub(crate) tags: Vec<String>,
    /// 给库主看的「添加理由」，见 MemeOrigin::reason。缺省允许为空。
    #[serde(default)]
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PositiveGates {
    pub(crate) chat_reaction: bool,
    pub(crate) emotion_or_meme: bool,
    pub(crate) reusable: bool,
    pub(crate) context_independent: bool,
    pub(crate) persona_fit: bool,
    pub(crate) meaning_clear: bool,
    pub(crate) visual_quality: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RiskGates {
    pub(crate) ordinary_photo: bool,
    pub(crate) informational_content: bool,
    pub(crate) privacy: bool,
    pub(crate) advertisement: bool,
    pub(crate) unsafe_or_abusive: bool,
}

pub(crate) async fn add_meme(args: Value, config: &AppConfig, paths: &MiyuPaths) -> Result<String> {
    let library = selected_library(&args, config);
    let library_lock = library_lock(&library);
    let _guard = library_lock.lock().await;
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
    let format = validate_image_bytes(&bytes)?;
    let ext = format.extension();
    let mime_type = format.mime().to_string();
    let animated = format == ValidatedImageFormat::Gif;
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
        match item_from_args(
            &args,
            id.clone(),
            format!("images/{target_file}"),
            mime_type,
            animated,
        ) {
            Ok(item) => item,
            Err(error) => {
                let _ = std::fs::remove_file(&target);
                return Err(error);
            }
        }
    } else {
        match classify_meme_image(config, paths, &target).await {
            Ok(classification) => match item_from_classification(
                id.clone(),
                format!("images/{target_file}"),
                mime_type,
                animated,
                classification,
                None,
            ) {
                Ok(item) => item,
                Err(err) => {
                    let _ = std::fs::remove_file(&target);
                    return Ok(json!({
                        "success": false,
                        "rejected": true,
                        "message": "vision classification rejected the image",
                        "error": err.to_string(),
                    })
                    .to_string());
                }
            },
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
    if let Err(error) = save_index(&user_dir.join("index.json"), &index) {
        let _ = std::fs::remove_file(&target);
        return Err(error);
    }
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

pub(crate) async fn update_meme(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    let library = selected_library(&args, config);
    let library_lock = library_lock(&library);
    let _guard = library_lock.lock().await;
    let id = required_str(&args, "id")?;
    let existing =
        find_meme_any(paths, &library, id)?.with_context(|| format!("meme not found: {id}"))?;
    let id = existing.item.id.clone();
    let user_dir = user_library_dir(paths, &library);
    if matches!(existing.source, MemeSource::Builtin) {
        // 内置条目的 file 是相对内置目录的路径:落进用户 overlay 前必须
        // 把图片实体复制过来,否则之后按 user_dir 解析落空,show 必失败。
        let target = user_dir.join(&existing.item.file);
        if !target.exists() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&existing.path, &target).with_context(|| {
                format!(
                    "failed to copy builtin meme image {} to {}",
                    existing.path.display(),
                    target.display()
                )
            })?;
        }
    }
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

pub(crate) async fn delete_meme(
    args: Value,
    config: &AppConfig,
    paths: &MiyuPaths,
) -> Result<String> {
    let library = selected_library(&args, config);
    let library_lock = library_lock(&library);
    let _guard = library_lock.lock().await;
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

pub(crate) async fn classify_meme_image(
    config: &AppConfig,
    paths: &MiyuPaths,
    image: &Path,
) -> Result<MemeClassification> {
    let persona = config.active_persona_prompt(paths).unwrap_or_default();
    let persona = persona.chars().take(4_000).collect::<String>();
    let prompt = if persona.trim().is_empty() {
        MEME_DESCRIPTION_PROMPT.to_string()
    } else {
        format!(
            "{MEME_DESCRIPTION_PROMPT}\n\n## 当前人格约束\n仅当图片明确符合以下人格时，persona_fit 才能为 true：\n{persona}"
        )
    };
    let text = vision::analyze_local_image_with_prompt(config, paths, image, &prompt).await?;
    let classification = parse_meme_classification(&text)?;
    validate_classification(&classification)?;
    Ok(classification)
}

/// 识图模型十有八九会把 JSON 裹进 ```json 围栏,或者先说一句"好的,分析如下"
/// —— 提示词里的 schema 示例本身就是围栏写法,等于在教它这么答。直解失败就
/// 从正文里抠出第一个完整对象再试一次,和 memory organizer / affection 那几处
/// 同构(全仓库只有这里漏了这层兜底)。
///
/// 报错必须带上 serde 的原话和响应开头:光一句"不是严格 schema"谁也定位不了
/// 是围栏、是少字段、还是模型压根在拒答。
pub(crate) fn parse_meme_classification(text: &str) -> Result<MemeClassification> {
    let trimmed = text.trim();
    let direct = match serde_json::from_str::<MemeClassification>(trimmed) {
        Ok(classification) => return Ok(classification),
        Err(error) => error,
    };
    let Some(extracted) = crate::json_extract::extract_json_object(trimmed) else {
        bail!(
            "vision response was not the strict meme schema ({direct}); response began: {}",
            meme_response_excerpt(trimmed)
        );
    };
    serde_json::from_str(extracted).map_err(|error| {
        anyhow!(
            "vision response was not the strict meme schema ({error}); response began: {}",
            meme_response_excerpt(trimmed)
        )
    })
}

fn meme_response_excerpt(text: &str) -> String {
    let excerpt: String = text.chars().take(200).collect();
    let excerpt = excerpt.replace('\n', " ");
    if text.chars().count() > 200 {
        format!("{excerpt}…")
    } else {
        excerpt
    }
}

pub(crate) async fn collect_meme_from_local_image(
    image: &Path,
    config: &AppConfig,
    paths: &MiyuPaths,
    origin: Option<MemeOrigin>,
) -> Result<MemeCollectionOutcome> {
    let library = current_persona_library(config);
    let image = image.to_path_buf();
    let max_bytes = config
        .plugins
        .memes
        .max_image_mb
        .saturating_mul(1024 * 1024);
    let prepared = match tokio::task::spawn_blocking(move || prepare_image(&image, max_bytes))
        .await
        .context("image validation task failed")?
    {
        Ok(prepared) => prepared,
        Err(error) => {
            return Ok(MemeCollectionOutcome::Rejected {
                reason: error.to_string(),
            })
        }
    };
    let meme_ref = MemeRef {
        library: library.clone(),
        id: prepared.id.clone(),
    };
    if find_meme(paths, &library, &prepared.id)?.is_some() {
        return Ok(MemeCollectionOutcome::AlreadyExists { meme: meme_ref });
    }

    let vision_input = tempfile::Builder::new()
        .suffix(&format!(".{}", prepared.format.extension()))
        .tempfile()?;
    std::fs::copy(&prepared.source, vision_input.path())?;
    let classification = match classify_meme_image(config, paths, vision_input.path()).await {
        Ok(classification) => classification,
        Err(error) => {
            return Ok(MemeCollectionOutcome::Rejected {
                reason: error.to_string(),
            })
        }
    };
    if !classification.save {
        return Ok(MemeCollectionOutcome::Rejected {
            reason: "vision classification rejected the image".to_string(),
        });
    }

    let lock = library_lock(&library);
    let _guard = lock.lock().await;
    if find_meme(paths, &library, &prepared.id)?.is_some() {
        return Ok(MemeCollectionOutcome::AlreadyExists { meme: meme_ref });
    }
    let user_dir = user_library_dir(paths, &library);
    let images_dir = user_dir.join("images");
    std::fs::create_dir_all(&images_dir)?;
    let target_file = format!("{}.{}", &prepared.hash[..16], prepared.format.extension());
    let target = images_dir.join(&target_file);
    std::fs::copy(&prepared.source, &target).with_context(|| {
        format!(
            "failed to copy image {} to {}",
            prepared.source.display(),
            target.display()
        )
    })?;
    let origin = origin.map(|mut origin| {
        origin.collected_at = chrono::Utc::now().to_rfc3339();
        origin
    });
    let item = match item_from_classification(
        prepared.id.clone(),
        format!("images/{target_file}"),
        prepared.format.mime().to_string(),
        prepared.format == ValidatedImageFormat::Gif,
        classification,
        origin,
    ) {
        Ok(item) => item,
        Err(error) => {
            let _ = std::fs::remove_file(&target);
            return Ok(MemeCollectionOutcome::Rejected {
                reason: error.to_string(),
            });
        }
    };
    let mut index = load_index(&user_dir.join("index.json"))?.unwrap_or_else(|| MemeIndex {
        library: library.clone(),
        version: 2,
        memes: Vec::new(),
        disabled_ids: Vec::new(),
    });
    index.library = library.clone();
    index.version = 2;
    index
        .disabled_ids
        .retain(|value| !ids_match(value, &prepared.id));
    // 之前被禁用的同 id 条目还留在 memes 里(find_meme 滤 disabled 看
    // 不见它):替换而不是追加,避免同 id 双条目。
    if let Some(existing) = index
        .memes
        .iter_mut()
        .find(|meme| ids_match(&meme.id, &prepared.id))
    {
        *existing = item;
    } else {
        index.memes.push(item);
    }
    if let Err(error) = save_index(&user_dir.join("index.json"), &index) {
        let _ = std::fs::remove_file(&target);
        return Err(error);
    }
    Ok(MemeCollectionOutcome::Accepted { meme: meme_ref })
}
