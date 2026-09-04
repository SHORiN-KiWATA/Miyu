//! WebUI dashboard 用的表情库视图与操作。
//!
//! 列表**不走** `load_library`:那条路把禁用项滤掉了,面板得看见四种状态
//! (内置 / 用户 / 影子覆盖 / 已禁用)。写操作全部复用 crud.rs 的入口,
//! 锁与索引落盘方式不另起一套。

use crate::tools::memes::*;

pub(crate) struct DashboardLibrary {
    pub(crate) name: String,
    pub(crate) has_builtin: bool,
    pub(crate) has_user: bool,
}

/// 磁盘上的用户库 ∪ 内置库 ∪ 当前人格映射到的库。
pub(crate) fn dashboard_libraries(config: &AppConfig, paths: &MiyuPaths) -> Result<Value> {
    let active = current_persona_library(config);
    let mut names: std::collections::BTreeMap<String, DashboardLibrary> =
        std::collections::BTreeMap::new();
    let mut note = |name: String, builtin: bool| {
        let entry = names.entry(name.clone()).or_insert(DashboardLibrary {
            name,
            has_builtin: false,
            has_user: false,
        });
        if builtin {
            entry.has_builtin = true;
        } else {
            entry.has_user = true;
        }
    };
    if let Ok(entries) = std::fs::read_dir(paths.data_dir.join("memes")) {
        for entry in entries.flatten() {
            if entry.path().join("index.json").is_file() {
                if let Some(name) = entry.file_name().to_str() {
                    note(name.to_string(), false);
                }
            }
        }
    }
    if let Some(parent) = builtin_library_dir("x").parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                if entry.path().join("index.json").is_file() {
                    if let Some(name) = entry.file_name().to_str() {
                        note(name.to_string(), true);
                    }
                }
            }
        }
    }
    let active_builtin = builtin_library_dir(&active).join("index.json").is_file();
    note(active.clone(), active_builtin);
    let libraries: Vec<Value> = names
        .values()
        .map(|library| {
            json!({
                "name": library.name,
                "builtin": library.has_builtin,
                "user": library.has_user,
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "active": active,
        "active_persona": config.prompt.active_persona,
        "persona_libraries": config.plugins.memes.persona_libraries,
        "libraries": libraries,
    }))
}

fn item_json(item: &MemeItem, source: MemeSource, shadowed: bool, disabled: bool, short_id: &str) -> Value {
    json!({
        "id": item.id,
        "short_id": short_id,
        "name": item.name,
        "file": item.file,
        "mime_type": item.mime_type,
        "animated": item.animated,
        "description": item.description,
        "usage": item.usage,
        "tags": item.tags,
        "origin": item.origin,
        "source": source_label(source),
        "shadowed": shadowed,
        "disabled": disabled,
    })
}

/// 全量列表:用户项在前,内置项在后;被用户影子盖住的内置项不列(与模型看到的
/// 一致,影子本身标 shadowed),禁用项照列并标 disabled。
pub(crate) fn dashboard_list(paths: &MiyuPaths, library: &str) -> Result<Value> {
    let library = sanitize_library(library);
    let builtin_dir = builtin_library_dir(&library);
    let user_dir = user_library_dir(paths, &library);
    let builtin_index = builtin_dir.join("index.json");
    let user_index = user_dir.join("index.json");
    let builtin = load_index(&builtin_index)?.unwrap_or_default();
    let user = load_index(&user_index)?.unwrap_or_default();
    let disabled = &user.disabled_ids;
    let is_disabled = |id: &str| disabled.iter().any(|value| ids_match(value, id));
    let builtin_ids: Vec<String> = builtin.memes.iter().map(|item| item.id.clone()).collect();
    let user_ids: Vec<String> = user.memes.iter().map(|item| item.id.clone()).collect();
    let mut all_ids = user_ids.clone();
    all_ids.extend(builtin_ids.iter().cloned());
    let mut items = Vec::new();
    let mut counts = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize); // builtin, user, shadowed, disabled, collected, collected_7d
    let week_ago = chrono::Utc::now() - chrono::Duration::days(7);
    for item in &user.memes {
        let shadowed = builtin_ids.iter().any(|id| ids_match(id, &item.id));
        let off = is_disabled(&item.id);
        counts.1 += 1;
        if shadowed {
            counts.2 += 1;
        }
        if off {
            counts.3 += 1;
        }
        if let Some(origin) = &item.origin {
            counts.4 += 1;
            if chrono::DateTime::parse_from_rfc3339(&origin.collected_at)
                .map(|at| at.with_timezone(&chrono::Utc) > week_ago)
                .unwrap_or(false)
            {
                counts.5 += 1;
            }
        }
        items.push(item_json(
            item,
            MemeSource::User,
            shadowed,
            off,
            &unique_short_id_from_ids(&all_ids, &item.id),
        ));
    }
    for item in &builtin.memes {
        if user_ids.iter().any(|id| ids_match(id, &item.id)) {
            continue;
        }
        let off = is_disabled(&item.id);
        counts.0 += 1;
        if off {
            counts.3 += 1;
        }
        items.push(item_json(
            item,
            MemeSource::Builtin,
            false,
            off,
            &unique_short_id_from_ids(&all_ids, &item.id),
        ));
    }
    Ok(json!({
        "ok": true,
        "library": library,
        "builtin_dir": builtin_dir.display().to_string(),
        "user_dir": user_dir.display().to_string(),
        "index_mtime": index_mtime(&user_index)
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        "items": items,
        "stats": {
            "total": items.len(),
            "builtin": counts.0,
            "user": counts.1,
            "shadowed": counts.2,
            "disabled": counts.3,
            "collected": counts.4,
            "collected_7d": counts.5,
        },
    }))
}

/// 缩略图 / 原图路由用:按 (库, id) 解析到磁盘路径,禁用项也能看。
pub(crate) fn dashboard_image(
    paths: &MiyuPaths,
    library: &str,
    id: &str,
) -> Result<Option<(PathBuf, String)>> {
    let Some(meme) = find_meme_any(paths, library, id)? else {
        return Ok(None);
    };
    Ok(Some((meme.path, meme.item.mime_type)))
}

pub(crate) struct DashboardUpload {
    pub(crate) library: String,
    pub(crate) name_zh: String,
    pub(crate) name_en: String,
    pub(crate) description: String,
    pub(crate) usage: String,
    pub(crate) tags: Vec<String>,
    /// true = 手填元数据;false = 交给视觉模型分类(可能被拒或失败)。
    pub(crate) manual: bool,
}

/// 浏览器上传:字节落临时文件,走 `add_meme` 同一条路(校验、去重、分类、落盘)。
pub(crate) async fn dashboard_add(
    config: &AppConfig,
    paths: &MiyuPaths,
    upload: DashboardUpload,
    bytes: &[u8],
) -> Result<Value> {
    if !config.plugins.memes.enabled {
        bail!("memes plugin is disabled in config")
    }
    let staging = paths.cache_dir.join("meme-uploads");
    std::fs::create_dir_all(&staging)?;
    let temp = tempfile::NamedTempFile::new_in(&staging)?;
    std::fs::write(temp.path(), bytes)?;
    let mut args = json!({
        "library": upload.library,
        "image": temp.path().display().to_string(),
    });
    if upload.manual {
        args["name_zh"] = json!(upload.name_zh);
        args["name_en"] = json!(upload.name_en);
        args["description"] = json!(upload.description);
        args["usage"] = json!(upload.usage);
        args["tags"] = json!(upload.tags);
    }
    let result = add_meme(args, config, paths).await?;
    let result: Value = serde_json::from_str(&result)?;
    Ok(result)
}

pub(crate) struct DashboardPatch {
    pub(crate) name_zh: Option<String>,
    pub(crate) name_en: Option<String>,
    pub(crate) description: Option<String>,
    pub(crate) usage: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) enabled: Option<bool>,
}

pub(crate) async fn dashboard_update(
    config: &AppConfig,
    paths: &MiyuPaths,
    library: &str,
    id: &str,
    patch: DashboardPatch,
) -> Result<Value> {
    // update_meme 不复查文本长度与标签合法性(08-16 已知缺口),这里先校验。
    if let Some(value) = patch.name_zh.as_deref() {
        validate_text_field("name_zh", value, 1, 80)?;
    }
    if let Some(value) = patch.name_en.as_deref().filter(|value| !value.is_empty()) {
        validate_text_field("name_en", value, 1, 80)?;
    }
    if let Some(value) = patch.description.as_deref() {
        validate_text_field("description", value, 1, 500)?;
    }
    if let Some(value) = patch.usage.as_deref() {
        validate_text_field("usage", value, 1, 500)?;
    }
    if let Some(tags) = patch.tags.as_ref() {
        validate_tags(tags, false)?;
    }
    let mut args = json!({ "library": library, "id": id });
    if let Some(value) = patch.name_zh {
        args["name_zh"] = json!(value);
    }
    if let Some(value) = patch.name_en {
        // 空串表示清掉英文名:apply_updates 会忽略空串,所以单独处理。
        args["name_en"] = json!(if value.is_empty() { " ".to_string() } else { value });
    }
    if let Some(value) = patch.description {
        args["description"] = json!(value);
    }
    if let Some(value) = patch.usage {
        args["usage"] = json!(value);
    }
    if let Some(value) = patch.tags {
        args["tags"] = json!(value);
    }
    if let Some(value) = patch.enabled {
        args["enabled"] = json!(value);
    }
    let result = update_meme(args, config, paths).await?;
    Ok(serde_json::from_str(&result)?)
}

pub(crate) async fn dashboard_delete(
    config: &AppConfig,
    paths: &MiyuPaths,
    library: &str,
    id: &str,
    hard: bool,
) -> Result<Value> {
    let result = delete_meme(
        json!({ "library": library, "id": id, "hard_delete": hard }),
        config,
        paths,
    )
    .await?;
    Ok(serde_json::from_str(&result)?)
}

/// 让视觉模型重新看一遍,只返回建议,不落盘——由前端决定采不采纳。
pub(crate) async fn dashboard_classify(
    config: &AppConfig,
    paths: &MiyuPaths,
    library: &str,
    id: &str,
) -> Result<Value> {
    let meme = find_meme_any(paths, library, id)?.with_context(|| format!("meme not found: {id}"))?;
    let classification = classify_meme_image(config, paths, &meme.path).await?;
    Ok(json!({
        "ok": true,
        "name": classification.name,
        "description": classification.description,
        "usage": classification.usage,
        "tags": classification.tags,
        "confidence": classification.confidence,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(temp: &tempfile::TempDir) -> MiyuPaths {
        MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    fn png_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        let img = image::RgbaImage::from_pixel(64, 48, image::Rgba([200, 30, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[tokio::test]
    async fn list_upload_update_disable_delete_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let config = AppConfig::default();
        // 空库:列表空、统计全零。
        let empty = dashboard_list(&paths, "testlib").unwrap();
        assert_eq!(empty["stats"]["total"], 0);

        let upload = DashboardUpload {
            library: "testlib".into(),
            name_zh: "红方块".into(),
            name_en: "red block".into(),
            description: "一块纯红色".into(),
            usage: "测试时发".into(),
            tags: vec!["测试".into()],
            manual: true,
        };
        let added = dashboard_add(&config, &paths, upload, &png_bytes()).await.unwrap();
        assert_eq!(added["success"], true);
        let id = added["id"].as_str().unwrap().to_string();

        let listed = dashboard_list(&paths, "testlib").unwrap();
        assert_eq!(listed["stats"]["user"], 1);
        assert_eq!(listed["items"][0]["source"], "user");
        assert_eq!(listed["items"][0]["disabled"], false);
        let (path, mime) = dashboard_image(&paths, "testlib", &id).unwrap().unwrap();
        assert!(path.is_file());
        assert_eq!(mime, "image/png");

        // 同一字节再传:already_exists,不重复入库。
        let again = DashboardUpload {
            library: "testlib".into(),
            name_zh: "x".into(),
            name_en: String::new(),
            description: "y".into(),
            usage: "z".into(),
            tags: vec![],
            manual: true,
        };
        let dup = dashboard_add(&config, &paths, again, &png_bytes()).await.unwrap();
        assert_eq!(dup["already_exists"], true);

        // 编辑 + 校验:超长描述拒绝;正常改写生效;禁用后列表仍列出并标 disabled。
        let too_long = DashboardPatch {
            name_zh: None,
            name_en: None,
            description: Some("x".repeat(600)),
            usage: None,
            tags: None,
            enabled: None,
        };
        assert!(dashboard_update(&config, &paths, "testlib", &id, too_long).await.is_err());
        let patch = DashboardPatch {
            name_zh: Some("大红方块".into()),
            name_en: None,
            description: None,
            usage: None,
            tags: Some(vec!["测试".into(), "红色".into()]),
            enabled: Some(false),
        };
        dashboard_update(&config, &paths, "testlib", &id, patch).await.unwrap();
        let listed = dashboard_list(&paths, "testlib").unwrap();
        assert_eq!(listed["items"][0]["name"]["zh"], "大红方块");
        assert_eq!(listed["items"][0]["tags"], json!(["测试", "红色"]));
        assert_eq!(listed["items"][0]["disabled"], true);
        assert_eq!(listed["stats"]["disabled"], 1);
        assert!(dashboard_image(&paths, "testlib", &id).unwrap().is_some());

        let deleted = dashboard_delete(&config, &paths, "testlib", &id, true).await.unwrap();
        assert_eq!(deleted["action"], "deleted_user_meme");
        assert_eq!(dashboard_list(&paths, "testlib").unwrap()["stats"]["total"], 0);
        assert!(dashboard_image(&paths, "testlib", &id).unwrap().is_none());

        let libraries = dashboard_libraries(&config, &paths).unwrap();
        assert!(libraries["libraries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "testlib" && entry["user"] == true));
    }
}
