use super::{ToolRegistry, ToolSpec};
use crate::config::AppConfig;
use crate::i18n::text as t;
use crate::market;
use crate::paths::MiyuPaths;
use anyhow::Result;
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    register_readonly(registry, config.clone(), paths.clone());
    registry.register(
        ToolSpec::new(
            "market_install",
            t(
                "Install a persona or identity from the Miyu market. Use this when the user wants to download and use a persona or identity they found in the market.",
                "从 Miyu 人格市场安装人格或角色。当用户想要下载使用市场中的人格或角色时使用。"
            ),
            json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": t(
                            "\"persona\" for AI personality prompts, \"identity\" for user identity profiles",
                            "\"persona\" 表示人格提示词，\"identity\" 表示用户身份档案"
                        )
                    },
                    "name": {
                        "type": "string",
                        "description": t(
                            "The persona or identity name (filename without .md extension)",
                            "人格或角色名称（不含 .md 后缀）"
                        )
                    }
                },
                "required": ["kind", "name"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move { install_tool(args, config, paths).await }
                }
            },
        )
        .writes(),
    );
    registry.register(
        ToolSpec::new(
            "market_update",
            t(
                "Update installed personas or identities from the Miyu market. Use when the user wants to check for updates or refresh an installed persona/identity. Omit name to update all installed.",
                "从 Miyu 人格市场更新已安装的人格或角色。当用户想要检查更新或刷新已安装内容时使用。省略名称则更新全部。"
            ),
            json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "description": t(
                            "\"personas\" to update personas, \"identities\" to update identities",
                            "\"personas\" 更新人格，\"identities\" 更新角色"
                        )
                    },
                    "name": {
                        "type": "string",
                        "description": t(
                            "Specific persona/identity name to update. Omit to update all installed.",
                            "指定要更新的人格/角色名称。省略则更新全部已安装。"
                        )
                    }
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
            {
                let config = config.clone();
                let paths = paths.clone();
                move |args| {
                    let config = config.clone();
                    let paths = paths.clone();
                    async move { update_tool(args, config, paths).await }
                }
            },
        )
        .writes(),
    );
}

pub fn register_readonly(registry: &mut ToolRegistry, config: AppConfig, paths: MiyuPaths) {
    registry.register(ToolSpec::new(
        "market_list",
        t(
            "List available personas or identities from the Miyu market. Use when the user wants to browse what personas or identities are available.",
            "列出 Miyu 人格市场中可用的人格或角色。当用户想要浏览可安装的人格或角色时使用。"
        ),
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": t(
                        "\"personas\" to list AI personas, \"identities\" to list user identities",
                        "\"personas\" 列出人格，\"identities\" 列出用户角色"
                    )
                }
            },
            "required": ["kind"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            let paths = paths.clone();
            move |args| {
                let config = config.clone();
                let paths = paths.clone();
                async move { list_tool(args, config, paths).await }
            }
        },
    ));
    registry.register(ToolSpec::new(
        "market_show",
        t(
            "Show the full content of a persona or identity from the Miyu market. Use when the user wants to preview before installing.",
            "显示 Miyu 市场中某个人格或角色的完整内容。当用户想要在安装前预览时使用。"
        ),
        json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": t(
                        "\"persona\" for AI persona, \"identity\" for user identity",
                        "\"persona\" 表示人格，\"identity\" 表示用户角色"
                    )
                },
                "name": {
                    "type": "string",
                    "description": t(
                        "The persona or identity name (filename without .md)",
                        "人格或角色名称（不含 .md 后缀）"
                    )
                }
            },
            "required": ["kind", "name"],
            "additionalProperties": false
        }),
        {
            let config = config.clone();
            move |args| {
                let config = config.clone();
                async move { show_tool(args, config).await }
            }
        },
    ));
}

async fn list_tool(args: Value, config: AppConfig, _paths: MiyuPaths) -> Result<String> {
    let kind = args["kind"].as_str().unwrap_or("personas");
    let dir = market::resolve_dir(kind)?;
    let items = market::fetch_contents_listing(&config.market, dir).await?;
    let mut results = Vec::new();
    let mut fetch_errors = 0usize;
    for item_json in &items {
        let filename = item_json["name"].as_str().unwrap_or_default();
        let raw = match market::fetch_raw(&config.market, dir, filename).await {
            Ok(r) => r,
            Err(_) => {
                fetch_errors += 1;
                continue;
            }
        };
        let item = market::parse_item(filename, &raw);
        results.push(json!({
            "name": item.name,
            "author": item.author,
            "description": item.description,
            "version": item.version,
            "tags": item.tags,
        }));
    }
    let mut out = json!({ "items": results });
    if fetch_errors > 0 {
        out["fetch_errors"] = json!(fetch_errors);
    }
    if results.is_empty() {
        if items.is_empty() {
            return Ok(t(
                "No personas available in the market.",
                "市场中暂无可用人格。"
            ).to_string());
        }
        return Ok(t(
            "Failed to fetch any persona content. Check network or try again later.",
            "获取人格内容全部失败，请检查网络或稍后重试。"
        ).to_string());
    }
    Ok(serde_json::to_string_pretty(&out)?)
}

async fn show_tool(args: Value, config: AppConfig) -> Result<String> {
    let kind = args["kind"].as_str().unwrap_or("persona");
    let name = args["name"].as_str().unwrap_or_default();
    let dir = market::resolve_dir(kind)?;
    let filename = market::resolve_filename(name)?;
    let raw = market::fetch_raw(&config.market, dir, &filename).await?;
    Ok(raw)
}

async fn install_tool(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    let kind = args["kind"].as_str().unwrap_or("persona");
    let name = args["name"].as_str().unwrap_or_default();
    let dir = market::resolve_dir(kind)?;
    let filename = market::resolve_filename(name)?;
    let raw = market::fetch_raw(&config.market, dir, &filename).await?;
    let version = crate::tools::skills::frontmatter_value(&raw, "version")
        .unwrap_or_else(|| "0.0.0".to_string());
    let dest_dir = market::target_dir(kind, &config, &paths)?;
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(&filename);
    let overwritten = dest.exists();
    std::fs::write(&dest, &raw)?;
    let mut state = market::load_state(&paths)?;
    state.items.insert(
        format!("{}/{}", dir, filename),
        market::MarketStateEntry {
            dir: dir.to_string(),
            version: version.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    market::save_state(&paths, &state)?;
    let action = if overwritten {
        t("Reinstalled (overwritten)", "已重新安装（覆盖）")
    } else {
        t("Installed", "已安装")
    };
    Ok(format!("{action} {name} v{version}"))
}

async fn update_tool(args: Value, config: AppConfig, paths: MiyuPaths) -> Result<String> {
    let kind = args["kind"].as_str().unwrap_or("personas");
    let name = args["name"].as_str().unwrap_or_default();
    let dir = market::resolve_dir(kind)?;
    let mut state = market::load_state(&paths)?;
    let prefix = format!("{}/", dir);
    let dest_dir = market::target_dir(kind, &config, &paths)?;
    let targets: Vec<String> = if !name.is_empty() {
        let filename = market::resolve_filename(name)?;
        let key = format!("{}/{}", dir, filename);
        if state.items.contains_key(&key) {
            vec![key]
        } else {
            return Ok(t(
                "Not installed, nothing to update.",
                "未安装，没有可更新的内容。"
            ).to_string());
        }
    } else {
        state
            .items
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect()
    };
    if targets.is_empty() {
        return Ok(t(
            "No installed personas to update.",
            "没有可更新的已安装人格。"
        ).to_string());
    }
    let mut updated = Vec::new();
    let mut skipped = Vec::new();
    for key in &targets {
        let filename = key.strip_prefix(&prefix).unwrap_or(key).to_string();
        let raw = match market::fetch_raw(&config.market, dir, &filename).await {
            Ok(r) => r,
            Err(_) => {
                skipped.push(filename);
                continue;
            }
        };
        let remote_version = crate::tools::skills::frontmatter_value(&raw, "version")
            .unwrap_or_else(|| "0.0.0".to_string());
        let installed_version = state
            .items
            .get(key)
            .map(|e| e.version.clone())
            .unwrap_or_default();
        if remote_version == installed_version {
            continue;
        }
        let entry = match state.items.get_mut(key) {
            Some(e) => e,
            None => continue,
        };
        std::fs::create_dir_all(&dest_dir)?;
        std::fs::write(dest_dir.join(&filename), &raw)?;
        entry.version = remote_version.clone();
        entry.installed_at = chrono::Utc::now().to_rfc3339();
        updated.push(format!("{filename}: v{installed_version} -> v{remote_version}"));
    }
    market::save_state(&paths, &state)?;
    Ok(serde_json::to_string_pretty(&json!({
        "updated": updated,
        "skipped": skipped,
    }))?)
}
