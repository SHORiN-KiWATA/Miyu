use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const MAX_PAGE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_CHARS: usize = 20_000;

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "query_moegirl",
        "Search or read Moegirlpedia pages. Supports zh/cn, uk, and ja sites.",
        json!({"type":"object","properties":{"query":{"type":"string"},"title":{"type":"string"},"mode":{"type":"string","enum":["auto","search","page"]},"site":{"type":"string","enum":["zh","cn","uk","ja",""]}},"additionalProperties":false}),
        |args| async move { query(args).await },
    ));
}

async fn query(args: Value) -> Result<String> {
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    let site = site(args.get("site").and_then(Value::as_str).unwrap_or("zh"));
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if mode == "search" || (mode == "auto" && title.is_empty()) {
        let q = if query.is_empty() { title } else { query };
        if q.is_empty() {
            bail!("query or title is required")
        }
        let url = format!(
            "{}/api.php?action=opensearch&search={}&limit=5&namespace=0&format=json",
            site.api,
            urlencoding::encode(q)
        );
        let data: Value = client()
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if mode == "search" {
            return Ok(serde_json::to_string_pretty(&data)?);
        }
        if let Some(first) = data
            .get(1)
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
        {
            return fetch_page(site, first).await;
        }
    }
    fetch_page(site, if title.is_empty() { query } else { title }).await
}

async fn fetch_page(site: Site, title: &str) -> Result<String> {
    if title.trim().is_empty() {
        bail!("query or title is required")
    }
    let url = format!(
        "{}/rest.php/v1/page/{}/html",
        site.base,
        urlencoding::encode(title)
    );
    let html = match client().get(&url).send().await {
        Ok(response) => match limited_text(response.error_for_status()?).await {
            Ok(text) => text,
            Err(_) => fetch_page_via_api(site, title).await?,
        },
        Err(_) => fetch_page_via_api(site, title).await?,
    };
    Ok(format!(
        "Source: {}{}\n\n{}",
        site.page,
        urlencoding::encode(title),
        clip(&html2md::parse_html(&html))
    ))
}

async fn fetch_page_via_api(site: Site, title: &str) -> Result<String> {
    let url = format!(
        "{}/api.php?action=parse&page={}&prop=text&format=json",
        site.api,
        urlencoding::encode(title)
    );
    let data: Value = client()
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let html = data
        .pointer("/parse/text/*")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if html.trim().is_empty() {
        bail!("Moegirlpedia page not found or returned empty content")
    }
    Ok(html)
}

async fn limited_text(response: reqwest::Response) -> Result<String> {
    if response.content_length().unwrap_or(0) > MAX_PAGE_BYTES as u64 {
        bail!("Moegirlpedia page too large")
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_PAGE_BYTES {
        bail!("Moegirlpedia page too large")
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn clip(value: &str) -> String {
    if value.chars().count() <= MAX_OUTPUT_CHARS {
        value.to_string()
    } else {
        format!(
            "{}\n...[truncated to {MAX_OUTPUT_CHARS} chars]",
            value.chars().take(MAX_OUTPUT_CHARS).collect::<String>()
        )
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("miyu/0.1")
        .build()
        .expect("valid reqwest client")
}

#[derive(Clone, Copy)]
struct Site {
    api: &'static str,
    base: &'static str,
    page: &'static str,
}

fn site(value: &str) -> Site {
    match value {
        "uk" => Site {
            api: "https://moegirl.uk",
            base: "https://moegirl.uk",
            page: "https://moegirl.uk/",
        },
        "ja" => Site {
            api: "https://ja.moegirl.org",
            base: "https://ja.moegirl.org",
            page: "https://ja.moegirl.org/",
        },
        _ => Site {
            api: "https://zh.moegirl.org.cn",
            base: "https://zh.moegirl.org.cn",
            page: "https://zh.moegirl.org.cn/",
        },
    }
}
