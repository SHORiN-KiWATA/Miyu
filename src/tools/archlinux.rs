use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new("aur_search_packages", "Search AUR packages via official RPC.", json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"},"search_by":{"type":"string"}},"required":["query"],"additionalProperties":false}), |args| async move { aur_search(args).await }));
    registry.register(ToolSpec::new("aur_get_package_info", "Get AUR package information via official RPC.", json!({"type":"object","properties":{"package_name":{"type":"string"}},"required":["package_name"],"additionalProperties":false}), |args| async move { aur_info(args).await }));
    registry.register(ToolSpec::new("archlinux_official_package_query", "Query official Arch Linux package database. Supports search and exact package details. / 查询 Arch Linux 官方软件包数据库，支持搜索和精确包详情。", json!({"type":"object","properties":{"package_name":{"type":"string","description":"Package name. / 包名。"},"repo":{"type":"string","description":"Repository for detail mode, e.g. core or extra. / 详情模式的仓库，例如 core 或 extra。"},"arch":{"type":"string","description":"Architecture for detail mode, default x86_64. / 详情模式架构，默认 x86_64。"},"mode":{"type":"string","enum":["auto","search","detail"],"description":"auto uses detail when repo is provided, otherwise search. / auto 在提供 repo 时查详情，否则搜索。"}},"required":["package_name"],"additionalProperties":false}), |args| async move { official_package_query(args).await }));
    registry.register(ToolSpec::new(
        "aur_check_status",
        "Check Arch Linux service status.",
        super::empty_parameters(),
        |_| async move { arch_status().await },
    ));
    registry.register(ToolSpec::new("archwiki_query", "Search or read ArchWiki pages.", json!({"type":"object","properties":{"query":{"type":"string"},"title":{"type":"string"},"mode":{"type":"string","enum":["auto","search","page"]}},"additionalProperties":false}), |args| async move { archwiki(args).await }));
}

async fn official_package_query(args: Value) -> Result<String> {
    let package = required(&args, "package_name")?;
    let repo = args
        .get("repo")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let arch = args
        .get("arch")
        .and_then(Value::as_str)
        .unwrap_or("x86_64")
        .trim();
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    let mode = if mode == "auto" && !repo.is_empty() {
        "detail"
    } else if mode == "auto" {
        "search"
    } else {
        mode
    };
    let url = match mode {
        "detail" => {
            if repo.is_empty() {
                bail!("repo is required for detail mode")
            }
            format!(
                "https://archlinux.org/packages/{}/{}/{}/json/",
                urlencoding::encode(repo),
                urlencoding::encode(arch),
                urlencoding::encode(&package)
            )
        }
        "search" => format!(
            "https://archlinux.org/packages/search/json/?name={}",
            urlencoding::encode(&package)
        ),
        _ => bail!("mode must be auto, search, or detail"),
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("miyu-archlinux-official-package-query/0.1")
        .build()?;
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "Arch official package API returned HTTP {} for {}",
            status,
            url
        )
    }
    let data: Value = resp.json().await?;
    Ok(serde_json::to_string_pretty(&json!({
        "success": true,
        "mode": mode,
        "package_name": package,
        "repo": if repo.is_empty() { Value::Null } else { json!(repo) },
        "arch": arch,
        "url": url,
        "data": data,
    }))?)
}

async fn aur_search(args: Value) -> Result<String> {
    let query = required(&args, "query")?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(50) as usize;
    let by = args
        .get("search_by")
        .and_then(Value::as_str)
        .unwrap_or("name-desc");
    let url = format!(
        "https://aur.archlinux.org/rpc/?v=5&type=search&by={}&arg={}",
        urlencoding::encode(by),
        urlencoding::encode(&query)
    );
    let data: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
    Ok(serde_json::to_string_pretty(
        &json!({"success": true, "query": query, "results": data.get("results").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().take(limit).collect::<Vec<_>>() }),
    )?)
}

async fn aur_info(args: Value) -> Result<String> {
    let names = required(&args, "package_name")?;
    let mut url = "https://aur.archlinux.org/rpc/?v=5&type=info".to_string();
    for name in names
        .split([',', ' '])
        .filter(|item| !item.trim().is_empty())
        .take(5)
    {
        url.push_str("&arg[]=");
        url.push_str(&urlencoding::encode(name.trim()));
    }
    let data: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
    Ok(serde_json::to_string_pretty(&data)?)
}

async fn arch_status() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let status_page = client
        .get("https://status.archlinux.org/")
        .send()
        .await?
        .status();
    let aur_rpc = client
        .get("https://aur.archlinux.org/rpc/?v=5&type=search&arg=pacman")
        .send()
        .await?
        .status();
    Ok(serde_json::to_string_pretty(&json!({
        "success": true,
        "status_page": {"url": "https://status.archlinux.org/", "http_status": status_page.as_u16(), "ok": status_page.is_success()},
        "aur_rpc": {"url": "https://aur.archlinux.org/rpc/", "http_status": aur_rpc.as_u16(), "ok": aur_rpc.is_success()},
        "note": "status.archlinux.org no longer exposes the old /api/v2/summary.json endpoint; this tool checks page and AUR RPC reachability."
    }))?)
}

async fn archwiki(args: Value) -> Result<String> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if mode == "search" || (mode == "auto" && title.is_empty()) {
        let q = if query.is_empty() { title } else { query };
        let url = format!("https://wiki.archlinux.org/api.php?action=opensearch&search={}&limit=8&namespace=0&format=json", urlencoding::encode(q));
        let data: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
        if mode == "search" {
            return Ok(serde_json::to_string_pretty(&data)?);
        }
        if let Some(first) = data
            .get(1)
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
        {
            return fetch_archwiki_page(first).await;
        }
    }
    fetch_archwiki_page(if title.is_empty() { query } else { title }).await
}

async fn fetch_archwiki_page(title: &str) -> Result<String> {
    if title.trim().is_empty() {
        bail!("query or title is required")
    }
    let url = format!(
        "https://wiki.archlinux.org/api.php?action=parse&page={}&prop=text&format=json",
        urlencoding::encode(title)
    );
    let data: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
    let html = data
        .pointer("/parse/text/*")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(html2md::parse_html(html))
}

fn required(args: &Value, key: &str) -> Result<String> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if value.is_empty() {
        bail!("{key} is required")
    } else {
        Ok(value.to_string())
    }
}
