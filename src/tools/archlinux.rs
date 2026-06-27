use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new("aur_search_packages", "Search AUR packages via official RPC.", json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"},"search_by":{"type":"string"}},"required":["query"],"additionalProperties":false}), |args| async move { aur_search(args).await }));
    registry.register(ToolSpec::new("aur_get_package_info", "Get AUR package information via official RPC.", json!({"type":"object","properties":{"package_name":{"type":"string"}},"required":["package_name"],"additionalProperties":false}), |args| async move { aur_info(args).await }));
    registry.register(ToolSpec::new(
        "aur_check_status",
        "Check Arch Linux service status.",
        super::empty_parameters(),
        |_| async move { arch_status().await },
    ));
    registry.register(ToolSpec::new("archwiki_query", "Search or read ArchWiki pages.", json!({"type":"object","properties":{"query":{"type":"string"},"title":{"type":"string"},"mode":{"type":"string","enum":["auto","search","page"]}},"additionalProperties":false}), |args| async move { archwiki(args).await }));
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
