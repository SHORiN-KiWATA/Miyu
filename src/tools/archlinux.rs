use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

const ARCH_STATUS_BASE_URL: &str = "https://status.archlinux.org";
const ARCH_STATUS_PAGE_ID: &str = "vmM5ruWEAB";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new("aur_search_packages", "Search AUR packages via official RPC.", json!({"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"},"search_by":{"type":"string"}},"required":["query"],"additionalProperties":false}), |args| async move { aur_search(args).await }));
    registry.register(ToolSpec::new("aur_get_package_info", "Get AUR package information via official RPC.", json!({"type":"object","properties":{"package_name":{"type":"string"}},"required":["package_name"],"additionalProperties":false}), |args| async move { aur_info(args).await }));
    registry.register(ToolSpec::new("archlinux_official_package_query", "Query official Arch Linux package database. Supports search and exact package details. / 查询 Arch Linux 官方软件包数据库，支持搜索和精确包详情。", json!({"type":"object","properties":{"package_name":{"type":"string","description":"Package name. / 包名。"},"repo":{"type":"string","description":"Repository for detail mode, e.g. core or extra. / 详情模式的仓库，例如 core 或 extra。"},"arch":{"type":"string","description":"Architecture for detail mode, default x86_64. / 详情模式架构，默认 x86_64。"},"mode":{"type":"string","enum":["auto","search","detail"],"description":"auto uses detail when repo is provided, otherwise search. / auto 在提供 repo 时查详情，否则搜索。"}},"required":["package_name"],"additionalProperties":false}), |args| async move { official_package_query(args).await }));
    registry.register(ToolSpec::new(
        "aur_check_status",
        "Check Arch Linux / AUR service status with detailed incident, degradation, and downtime info.",
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
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(limit)
        .map(|item| normalize_search_item(&item))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(
        &json!({"success": true, "query": query, "results": results}),
    )?)
}

async fn aur_info(args: Value) -> Result<String> {
    let names_raw = required(&args, "package_name")?;
    let names: Vec<String> = names_raw
        .split([',', ' '])
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .take(5)
        .collect();
    if names.is_empty() {
        bail!("package_name is required");
    }
    let mut url = "https://aur.archlinux.org/rpc/?v=5&type=info".to_string();
    for name in &names {
        url.push_str("&arg[]=");
        url.push_str(&urlencoding::encode(name));
    }
    let data: Value = reqwest::get(url).await?.error_for_status()?.json().await?;
    let raw_results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let found_names: Vec<String> = raw_results
        .iter()
        .filter_map(|item| item.get("Name").and_then(Value::as_str).map(String::from))
        .collect();
    let missing: Vec<String> = names
        .iter()
        .filter(|n| !found_names.iter().any(|f| f.eq_ignore_ascii_case(n)))
        .cloned()
        .collect();
    let results = raw_results
        .iter()
        .map(|item| normalize_info_item(item))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&json!({
        "success": true,
        "requested": names,
        "found": found_names,
        "missing": missing,
        "results": results,
    }))?)
}

async fn arch_status() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("miyu-arch-status/0.1")
        .build()?;

    let event_url = format!(
        "{}/api/getEventFeed/{}",
        ARCH_STATUS_BASE_URL, ARCH_STATUS_PAGE_ID
    );
    let monitor_url = format!(
        "{}/api/getMonitorList/{}",
        ARCH_STATUS_BASE_URL, ARCH_STATUS_PAGE_ID
    );

    let (event_resp, monitor_resp) = tokio::try_join!(
        client.get(&event_url).send(),
        client.get(&monitor_url).send()
    )?;

    let event_data: Value = event_resp.error_for_status()?.json().await?;
    let monitor_data: Value = monitor_resp.error_for_status()?.json().await?;

    let events = event_data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let monitors = monitor_data
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let latest_event = events.first().map(|e| normalize_status_event(e));
    let aur_monitor = find_aur_monitor(&monitors);
    let monitor_id = aur_monitor
        .and_then(|m| m.get("monitorId"))
        .and_then(Value::as_u64);

    let monitor_detail = if let Some(mid) = monitor_id {
        let detail_url = format!(
            "{}/api/getMonitor/{}?m={}",
            ARCH_STATUS_BASE_URL, ARCH_STATUS_PAGE_ID, mid
        );
        let resp = client.get(&detail_url).send().await?;
        if resp.status().is_success() {
            resp.json::<Value>().await.unwrap_or(json!({}))
        } else {
            json!({})
        }
    } else {
        json!({})
    };

    let detail_monitor = monitor_detail.get("monitor").cloned().unwrap_or(json!({}));
    let monitor_status = detail_monitor
        .get("statusClass")
        .and_then(Value::as_str)
        .or_else(|| {
            aur_monitor
                .and_then(|m| m.get("statusClass"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default();
    let current_state = normalize_current_state(monitor_status);
    let (is_degraded, degraded_reason) =
        derive_degraded_state(monitor_status, latest_event.as_ref());

    let logs = detail_monitor
        .get("logs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let latest_down = find_latest_down(&logs);

    Ok(serde_json::to_string_pretty(&json!({
        "success": true,
        "current_state": current_state,
        "is_degraded": is_degraded,
        "degraded_reason": degraded_reason,
        "latest_down": latest_down,
        "latest_event": latest_event,
        "monitor": {
            "name": aur_monitor.and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("AUR"),
            "status_class": monitor_status,
            "monitor_id": monitor_id,
        },
        "source": ARCH_STATUS_BASE_URL,
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

fn as_list(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Array(arr) => arr.clone(),
        _ => vec![value.clone()],
    }
}

fn format_timestamp(value: &Value) -> Option<String> {
    let ts = value.as_i64()?;
    let dt = chrono::DateTime::from_timestamp(ts, 0)?;
    Some(dt.to_rfc3339())
}

fn normalize_search_item(item: &Value) -> Value {
    let name = item.get("Name").and_then(Value::as_str).unwrap_or_default();
    let last_mod = item.get("LastModified").unwrap_or(&Value::Null);
    json!({
        "name": name,
        "package_base": item.get("PackageBase"),
        "version": item.get("Version"),
        "description": item.get("Description"),
        "votes": item.get("NumVotes"),
        "popularity": item.get("Popularity"),
        "maintainer": item.get("Maintainer"),
        "out_of_date": item.get("OutOfDate").map(|v| !v.is_null()).unwrap_or(false),
        "out_of_date_at": item.get("OutOfDate"),
        "last_modified": item.get("LastModified"),
        "last_modified_iso": format_timestamp(last_mod),
        "upstream_url": item.get("URL"),
        "aur_url": if name.is_empty() { Value::Null } else { json!(format!("https://aur.archlinux.org/packages/{name}")) },
    })
}

fn normalize_info_item(item: &Value) -> Value {
    let name = item.get("Name").and_then(Value::as_str).unwrap_or_default();
    let last_mod = item.get("LastModified").unwrap_or(&Value::Null);
    let first_sub = item.get("FirstSubmitted").unwrap_or(&Value::Null);
    json!({
        "name": name,
        "package_base": item.get("PackageBase"),
        "version": item.get("Version"),
        "description": item.get("Description"),
        "votes": item.get("NumVotes"),
        "popularity": item.get("Popularity"),
        "maintainer": item.get("Maintainer"),
        "out_of_date": item.get("OutOfDate").map(|v| !v.is_null()).unwrap_or(false),
        "out_of_date_at": item.get("OutOfDate"),
        "first_submitted": item.get("FirstSubmitted"),
        "first_submitted_iso": format_timestamp(first_sub),
        "last_modified": item.get("LastModified"),
        "last_modified_iso": format_timestamp(last_mod),
        "upstream_url": item.get("URL"),
        "aur_url": if name.is_empty() { Value::Null } else { json!(format!("https://aur.archlinux.org/packages/{name}")) },
        "url_path": item.get("URLPath"),
        "license": as_list(item.get("License").unwrap_or(&Value::Null)),
        "keywords": as_list(item.get("Keywords").unwrap_or(&Value::Null)),
        "depends": as_list(item.get("Depends").unwrap_or(&Value::Null)),
        "make_depends": as_list(item.get("MakeDepends").unwrap_or(&Value::Null)),
        "check_depends": as_list(item.get("CheckDepends").unwrap_or(&Value::Null)),
        "opt_depends": as_list(item.get("OptDepends").unwrap_or(&Value::Null)),
        "provides": as_list(item.get("Provides").unwrap_or(&Value::Null)),
        "conflicts": as_list(item.get("Conflicts").unwrap_or(&Value::Null)),
    })
}

fn find_aur_monitor(monitors: &[Value]) -> Option<&Value> {
    for monitor in monitors {
        let name = monitor
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let url = monitor
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        if name == "aur" || url.contains("aur.archlinux.org") {
            return Some(monitor);
        }
    }
    monitors.first()
}

fn normalize_current_state(status_class: &str) -> &'static str {
    match status_class.to_lowercase().as_str() {
        "success" => "up",
        "danger" | "down" | "error" => "down",
        _ => "unknown",
    }
}

fn derive_degraded_state(
    monitor_status: &str,
    latest_event: Option<&Value>,
) -> (bool, Option<String>) {
    let status = monitor_status.to_lowercase();
    if let Some(event) = latest_event {
        let is_active = event
            .get("is_active")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = event
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let title = event
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let affected: Vec<String> = event
            .get("affected_services")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let aur_affected = affected
            .iter()
            .any(|s| s.to_lowercase().contains("aur.archlinux.org") || s.to_lowercase() == "aur");
        let mentions_aur = content.contains("aur.archlinux.org")
            || title.contains("aur")
            || content.contains("aur");
        if is_active && (aur_affected || mentions_aur) {
            return (
                true,
                Some("Arch status page has an unresolved incident affecting AUR".to_string()),
            );
        }
    }
    if status == "warning" || status == "degraded" {
        return (
            true,
            Some("AUR monitor status is not fully healthy".to_string()),
        );
    }
    (false, None)
}

fn normalize_status_event(item: &Value) -> Value {
    let content = item
        .get("content")
        .or_else(|| item.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ended_at = item
        .get("endDateGMT")
        .or_else(|| item.get("endDate"))
        .cloned();
    let is_active = ended_at.as_ref().map(|v| v.is_null()).unwrap_or(true);
    let timestamp = item.get("timestamp").unwrap_or(&Value::Null);
    json!({
        "title": item.get("title"),
        "type": item.get("type"),
        "event_type": item.get("eventType"),
        "is_active": is_active,
        "started_at": format_timestamp(timestamp),
        "started_at_raw": item.get("timeGMT"),
        "ended_at": ended_at,
        "content": content,
        "status": item.get("status"),
        "affected_services": extract_affected_services(content),
    })
}

fn extract_affected_services(content: &str) -> Vec<String> {
    let mut services = Vec::new();
    let mut capture = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.to_lowercase().starts_with("affected services:") {
            capture = true;
            continue;
        }
        if capture && line.starts_with('-') {
            services.push(line.trim_start_matches("- ").trim().to_string());
            continue;
        }
        if capture && !services.is_empty() {
            break;
        }
    }
    services
}

fn find_latest_down(logs: &[Value]) -> Option<Value> {
    for item in logs {
        let label = item
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let class = item
            .get("class")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        if label != "down" && class != "danger" {
            continue;
        }
        let reason_detail = item
            .get("reason")
            .and_then(|r| r.get("detail"))
            .and_then(|d| d.get("short"))
            .and_then(Value::as_str);
        return Some(json!({
            "started_at": item.get("dateGMTISO").or_else(|| item.get("timeGMT")),
            "duration": item.get("duration"),
            "reason": reason_detail,
        }));
    }
    None
}
