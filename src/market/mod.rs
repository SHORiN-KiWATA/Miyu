use crate::config::{AppConfig, MarketConfig};
use crate::i18n::text as t;
use crate::paths::MiyuPaths;
use crate::tools::skills;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub(crate) struct MarketItem {
    pub(crate) name: String,
    pub(crate) author: String,
    pub(crate) description: String,
    pub(crate) version: String,
    pub(crate) tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MarketStateEntry {
    pub(crate) dir: String,
    pub(crate) version: String,
    pub(crate) installed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MarketState {
    pub(crate) items: BTreeMap<String, MarketStateEntry>,
}

pub(crate) fn resolve_dir(kind: &str) -> Result<&'static str> {
    match kind.to_ascii_lowercase().as_str() {
        "persona" | "personas" => Ok("personas"),
        "identity" | "identities" => Ok("identities"),
        _ => bail!(t(
            "expected persona(s) or identity(ies)",
            "应为 persona(s) 或 identity(ies)"
        )),
    }
}

pub(crate) fn target_dir(kind: &str, config: &AppConfig, paths: &MiyuPaths) -> Result<PathBuf> {
    match resolve_dir(kind)? {
        "personas" => Ok(config.prompts_dir_path(paths)),
        "identities" => Ok(config.identities_dir_path(paths)),
        _ => unreachable!(),
    }
}

fn api_url(config: &MarketConfig, dir: &str) -> String {
    format!(
        "https://api.github.com/repos/{}/contents/{}?ref={}",
        config.repo, dir, config.branch
    )
}

fn raw_url(config: &MarketConfig, dir: &str, filename: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        config.repo, config.branch, dir, filename
    )
}

fn apply_auth(req: reqwest::RequestBuilder, config: &MarketConfig) -> reqwest::RequestBuilder {
    if config.token.is_empty() {
        req.header("User-Agent", "miyu-market")
    } else {
        req.header("User-Agent", "miyu-market")
            .header("Authorization", format!("Bearer {}", config.token))
    }
}

fn format_http_error(status: reqwest::StatusCode, remaining: &str) -> String {
    let code = status.as_u16();
    if code == 403 && remaining == "0" {
        t(
            "GitHub API rate limit exceeded. Set a token in Market Repo config to increase limit, or wait.",
            "GitHub API 频率限制已用完。在 Market Repo 配置中设置 token 可提升额度，或等待重置。"
        ).to_string()
    } else if code == 403 {
        t(
            "GitHub API access denied (403). The repository may be private or the token may be invalid.",
            "GitHub API 拒绝访问 (403)。仓库可能是私有的，或 token 无效。"
        ).to_string()
    } else if code == 429 {
        t(
            "GitHub API rate limited (429). Wait or set a token in Market Repo config.",
            "GitHub API 请求过于频繁 (429)。请等待或设置 token。"
        ).to_string()
    } else {
        format!("HTTP {code}")
    }
}

fn rate_limit_remaining(resp: &reqwest::Response) -> String {
    resp.headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("?")
        .to_string()
}

pub(crate) async fn fetch_contents_listing(config: &MarketConfig, dir: &str) -> Result<Vec<serde_json::Value>> {
    let client = reqwest::Client::new();
    let req = client.get(api_url(config, dir));
    let resp = apply_auth(req, config).send().await?;
    if !resp.status().is_success() {
        bail!(format_http_error(resp.status(), &rate_limit_remaining(&resp)));
    }
    let body = resp.text().await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&body)
        .context(t("failed to parse market listing", "解析市场列表失败"))?;
    Ok(items
        .into_iter()
        .filter(|item| {
            item["type"].as_str() == Some("file")
                && item["name"]
                    .as_str()
                    .map_or(false, |n| n.ends_with(".md"))
        })
        .collect())
}

pub(crate) async fn fetch_raw(config: &MarketConfig, dir: &str, filename: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let req = client.get(raw_url(config, dir, filename));
    let resp = apply_auth(req, config).send().await?;
    if resp.status().as_u16() == 404 {
        bail!(t(
            "Persona not found in market",
            "市场中未找到该人格"
        ));
    }
    if !resp.status().is_success() {
        bail!(format_http_error(resp.status(), &rate_limit_remaining(&resp)));
    }
    Ok(resp.text().await?)
}

fn state_file(paths: &MiyuPaths) -> PathBuf {
    paths.data_dir.join("market/state.json")
}

pub(crate) fn load_state(paths: &MiyuPaths) -> Result<MarketState> {
    let path = state_file(paths);
    if !path.is_file() {
        return Ok(MarketState {
            items: BTreeMap::new(),
        });
    }
    let raw = std::fs::read_to_string(&path)?;
    let path_display = path.display().to_string();
    Ok(serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {path_display}"))?)
}

pub(crate) fn save_state(paths: &MiyuPaths, state: &MarketState) -> Result<()> {
    let path = state_file(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn validate_filename(name: &str) -> Result<()> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name == "."
        || name.starts_with('.')
    {
        bail!(t("invalid file name", "无效的文件名"));
    }
    Ok(())
}

pub(crate) fn resolve_filename(name: &str) -> Result<String> {
    let trimmed = name.trim();
    validate_filename(trimmed)?;
    let resolved = if trimmed.ends_with(".md") {
        trimmed.to_string()
    } else {
        format!("{}.md", trimmed)
    };
    validate_filename(&resolved)?;
    Ok(resolved)
}

fn parse_tag_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub(crate) fn parse_item(filename: &str, raw: &str) -> MarketItem {
    let stem = filename.strip_suffix(".md").unwrap_or(filename);
    let name = skills::frontmatter_value(raw, "name").unwrap_or_else(|| stem.to_string());
    let author = skills::frontmatter_value(raw, "author").unwrap_or_else(|| "unknown".to_string());
    let description = skills::frontmatter_value(raw, "description").unwrap_or_else(|| {
        let body = skills::strip_frontmatter(raw);
        let first_line = body
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let desc: String = first_line.chars().take(80).collect();
        if first_line.chars().count() > 80 {
            format!("{desc}...")
        } else {
            desc
        }
    });
    let version = skills::frontmatter_value(raw, "version").unwrap_or_else(|| "0.0.0".to_string());
    let tags = skills::frontmatter_value(raw, "tags")
        .as_deref()
        .map(parse_tag_list)
        .unwrap_or_default();
    MarketItem {
        name,
        author,
        description,
        version,
        tags,
    }
}

pub async fn list(config: &MarketConfig, kind: &str) -> Result<()> {
    let dir = resolve_dir(kind)?;
    let items = fetch_contents_listing(config, dir).await?;
    if items.is_empty() {
        println!("{}", t("No personas available", "当前市场无可用人格"));
        return Ok(());
    }
    for item_json in &items {
        let filename = item_json["name"].as_str().unwrap_or_default().to_string();
        let raw = match fetch_raw(config, dir, &filename).await {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!(
                    "{} {}: {e}",
                    t("Skipping", "跳过"),
                    filename
                );
                continue;
            }
        };
        let item = parse_item(&filename, &raw);
        println!(
            "{}  by {}  v{}",
            item.name, item.author, item.version
        );
        if !item.description.is_empty() {
            println!("  {}", item.description);
        }
        if !item.tags.is_empty() {
            println!("  tags: {}", item.tags.join(", "));
        }
        println!();
    }
    Ok(())
}

pub async fn show(config: &MarketConfig, kind: &str, name: &str) -> Result<()> {
    let dir = resolve_dir(kind)?;
    let filename = resolve_filename(name)?;
    let raw = fetch_raw(config, dir, &filename).await?;
    println!("{}", raw);
    Ok(())
}

pub async fn install(
    config: &AppConfig,
    paths: &MiyuPaths,
    kind: &str,
    name: &str,
) -> Result<()> {
    let dir = resolve_dir(kind)?;
    let filename = resolve_filename(name)?;
    let raw = fetch_raw(&config.market, dir, &filename).await?;
    let version = skills::frontmatter_value(&raw, "version").unwrap_or_else(|| "0.0.0".to_string());
    let dest_dir = target_dir(kind, config, paths)?;
    std::fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(&filename);
    if dest.exists() {
        println!(
            "{} {}",
            t("Overwriting existing file", "覆盖已存在的文件"),
            dest.display()
        );
    }
    std::fs::write(&dest, &raw).with_context(|| {
        format!(
            "failed to write {}",
            dest.display()
        )
    })?;
    let display_name = name.strip_suffix(".md").unwrap_or(name);
    println!(
        "{} {} v{}",
        t("Installed", "已安装"),
        display_name,
        version
    );
    let mut state = load_state(paths)?;
    state.items.insert(
        format!("{}/{}", dir, filename),
        MarketStateEntry {
            dir: dir.to_string(),
            version,
            installed_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    save_state(paths, &state)?;
    Ok(())
}

pub async fn update(
    config: &AppConfig,
    paths: &MiyuPaths,
    kind: &str,
    name: Option<&str>,
) -> Result<()> {
    let dir = resolve_dir(kind)?;
    let mut state = load_state(paths)?;
    let prefix = format!("{}/", dir);
    let dest_dir = target_dir(kind, config, paths)?;
    let targets: Vec<String> = if let Some(name) = name {
        let filename = resolve_filename(name)?;
        let key = format!("{}/{}", dir, filename);
        if state.items.contains_key(&key) {
            vec![key]
        } else {
            println!(
                "{}: {}",
                t("Not installed, cannot update", "未安装，无法更新"),
                name
            );
            return Ok(());
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
        println!(
            "{}",
            t("No installed personas to update", "没有可更新的已安装人格")
        );
        return Ok(());
    }
    for key in &targets {
        let filename = key
            .strip_prefix(&prefix)
            .unwrap_or(key)
            .to_string();
        let raw = match fetch_raw(&config.market, dir, &filename).await {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!(
                    "{} {}: {e}",
                    t("Skipping", "跳过"),
                    filename
                );
                continue;
            }
        };
        let remote_version =
            skills::frontmatter_value(&raw, "version").unwrap_or_else(|| "0.0.0".to_string());
        let installed_version = state
            .items
            .get(key)
            .map(|e| e.version.clone())
            .unwrap_or_default();
        if remote_version == installed_version {
            println!(
                "{} (v{})",
                t("Already up to date", "已是最新"),
                filename
            );
            continue;
        }
        let entry = match state.items.get_mut(key) {
            Some(entry) => entry,
            None => continue,
        };
        std::fs::create_dir_all(&dest_dir)?;
        let dest = dest_dir.join(&filename);
        std::fs::write(&dest, &raw).with_context(|| {
            format!("failed to write {}", dest.display())
        })?;
        entry.version = remote_version.clone();
        entry.installed_at = chrono::Utc::now().to_rfc3339();
        println!(
            "{} {}: v{} -> v{}",
            t("Updated", "已更新"),
            filename,
            installed_version,
            remote_version
        );
    }
    save_state(paths, &state)?;
    Ok(())
}
