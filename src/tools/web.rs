use super::{ToolRegistry, ToolSpec};
use crate::config::WebPluginConfig;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use urlencoding::decode as url_decode;

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;
const DEFAULT_FETCH_MAX_CHARS: usize = 40_000;
const MAX_FETCH_CHARS: usize = 200_000;

const CRAWLER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const CRAWLER_TIMEOUT: Duration = Duration::from_secs(15);

static DDG_BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
static SOGOU_BLOCKED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

struct CrawlerResult {
    title: String,
    url: String,
    snippet: String,
    source: String,
}

pub fn register(registry: &mut ToolRegistry, config: WebPluginConfig) {
    register_search_tool(registry, "web_search", config.clone());
}

pub fn register_fetch(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "web_fetch",
        "Fetch a URL and return markdown, text, or html. Prefer this for opening a known URL. Does not search the web.",
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Fully-qualified http or https URL." },
                "format": { "type": "string", "enum": ["markdown", "text", "html"], "description": "Output format. Defaults to markdown." },
                "timeout": { "type": "integer", "description": "Timeout seconds, max 120." },
                "max_chars": { "type": "integer", "description": "Maximum characters to return. Defaults to 40000, max 200000." }
            },
            "required": ["url"],
            "additionalProperties": false
        }),
        |args| async move { web_fetch(args).await },
    ));
}

fn register_search_tool(registry: &mut ToolRegistry, name: &'static str, config: WebPluginConfig) {
    registry.register(ToolSpec::new(
        name,
        "Search the web. Prefer configured Tavily, Firecrawl, or AnySearch API keys; fallback to SearXNG, then built-in DuckDuckGo HTML search (with Yahoo/360/Sogou fallback) when providers fail.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "max_results": { "type": "integer", "description": "Maximum results; defaults to plugins.web.max_results." },
                "provider": { "type": "string", "enum": ["auto", "tavily", "firecrawl", "anysearch", "searxng", "script"], "description": "Search provider." }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        move |args| {
            let config = config.clone();
            async move { web_search(args, config).await }
        },
    ));
}

async fn web_search(args: Value, config: WebPluginConfig) -> Result<String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        bail!("query is required");
    }
    let max_results = args
        .get("max_results")
        .and_then(Value::as_u64)
        .unwrap_or(config.max_results as u64)
        .clamp(1, 10) as usize;
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let client = reqwest::Client::builder()
        .timeout(CRAWLER_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;
    let order: Vec<&str> = if provider == "auto" {
        vec!["tavily", "firecrawl", "anysearch", "searxng", "script"]
    } else {
        vec![provider]
    };
    for item in order {
        let result = match item {
            "tavily" => search_tavily(&client, query, max_results, &config.tavily_api_keys).await,
            "firecrawl" => {
                search_firecrawl(&client, query, max_results, &config.firecrawl_api_keys).await
            }
            "anysearch" => {
                search_anysearch(&client, query, max_results, &config.anysearch_api_keys).await
            }
            "searxng" => {
                search_searxng(&client, query, max_results, &config.searxng_base_url).await
            }
            "script" => search_duckduckgo(&client, query, max_results).await,
            _ => continue,
        };
        if let Ok(output) = result {
            if !output.trim().is_empty() {
                return Ok(output);
            }
        }
    }
    bail!("no web search provider succeeded; API keys missing/failed, SearXNG unavailable, and built-in DuckDuckGo fallback (with Yahoo/360/Sogou) returned no results")
}

async fn search_tavily(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
    keys: &[String],
) -> Result<String> {
    let Some(key) = keys.iter().find(|key| !key.trim().is_empty()) else {
        bail!("missing Tavily API key")
    };
    let payload = json!({"query": query, "max_results": max_results.min(20), "search_depth": "basic", "include_answer": false, "include_raw_content": "markdown"});
    let data: Value = client
        .post("https://api.tavily.com/search")
        .bearer_auth(key.trim())
        .json(&payload)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(format_search_results(
        query,
        "Tavily",
        data.get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    ))
}

async fn search_firecrawl(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
    keys: &[String],
) -> Result<String> {
    let Some(key) = keys.iter().find(|key| !key.trim().is_empty()) else {
        bail!("missing Firecrawl API key")
    };
    let payload = json!({"query": query, "limit": max_results.min(20), "sources": [{"type":"web"}], "scrapeOptions": {"formats": [{"type":"markdown"}], "onlyMainContent": true}});
    let data: Value = client
        .post("https://api.firecrawl.dev/v2/search")
        .bearer_auth(key.trim())
        .json(&payload)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let raw = data
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(format_search_results(query, "Firecrawl", raw))
}

async fn search_anysearch(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
    keys: &[String],
) -> Result<String> {
    let Some(key) = keys.iter().find(|key| !key.trim().is_empty()) else {
        bail!("missing AnySearch API key")
    };
    let payload = json!({"query": query, "max_results": max_results.min(20)});
    let data: Value = client
        .post("https://api.anysearch.com/v1/search")
        .bearer_auth(key.trim())
        .json(&payload)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(format_search_results(
        query,
        "AnySearch",
        data.get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    ))
}

async fn search_searxng(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
    base_url: &str,
) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        bail!("missing SearXNG base URL")
    }
    let url = format!(
        "{base_url}/search?q={}&format=json&language=auto&safesearch=0",
        urlencoding::encode(query)
    );
    let data: Value = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(max_results)
        .collect::<Vec<_>>();
    if results.is_empty() {
        bail!("SearXNG returned no results")
    }
    Ok(format_search_results(query, "SearXNG", results))
}

// ── Crawler helper functions ───────────────────────────────────

fn is_ddg_blocked() -> bool {
    DDG_BLOCKED_UNTIL
        .lock()
        .ok()
        .and_then(|guard| guard.filter(|&t| t > Instant::now()))
        .is_some()
}

fn set_ddg_blocked(duration: Duration) {
    if let Ok(mut guard) = DDG_BLOCKED_UNTIL.lock() {
        *guard = Some(Instant::now() + duration);
    }
}

fn is_sogou_blocked() -> bool {
    SOGOU_BLOCKED_UNTIL
        .lock()
        .ok()
        .and_then(|guard| guard.filter(|&t| t > Instant::now()))
        .is_some()
}

fn set_sogou_blocked(duration: Duration) {
    if let Ok(mut guard) = SOGOU_BLOCKED_UNTIL.lock() {
        *guard = Some(Instant::now() + duration);
    }
}

fn looks_like_ddg_challenge(status: u16, html: &str) -> bool {
    if !matches!(status, 200 | 202 | 403 | 429) {
        return false;
    }
    html.contains("bots use DuckDuckGo too")
        || html.contains("complete the following challenge")
        || html.contains("anomaly.js")
        || html.contains("Select all squares")
}

fn is_result_url_allowed(url: &str) -> bool {
    let lower = url.to_lowercase();
    let host = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or(&lower);
    let host = host.split('/').next().unwrap_or(host);
    if host.ends_with("duckduckgo.com") {
        return false;
    }
    if host == "search.yahoo.com" || host == "r.search.yahoo.com" {
        return false;
    }
    if host.ends_with("sogou.com") && lower.contains("/link") {
        return false;
    }
    if host.ends_with("so.com") && lower.contains("/link") {
        return false;
    }
    if host == "www.googleadservices.com"
        || host == "googleads.g.doubleclick.net"
        || host == "ad.doubleclick.net"
    {
        return false;
    }
    if host.ends_with("bing.com") && (lower.contains("/aclick") || lower.contains("/alink")) {
        return false;
    }
    true
}

fn dedupe_key(url: &str) -> String {
    let lower = url.to_lowercase();
    let stripped = lower.trim_end_matches('/');
    let no_scheme = stripped
        .strip_prefix("https://")
        .or_else(|| stripped.strip_prefix("http://"))
        .unwrap_or(stripped);
    if let Some(query_pos) = no_scheme.find('?') {
        no_scheme[..query_pos].to_string()
    } else {
        no_scheme.to_string()
    }
}

fn unwrap_ddg_url(url: &str) -> String {
    let url = html_unescape(url.trim());
    if let Some(q_pos) = url.find('?') {
        let query = &url[q_pos + 1..];
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("uddg=") {
                if let Ok(decoded) = url_decode(val) {
                    return decoded.to_string();
                }
                return val.to_string();
            }
        }
    }
    if url.starts_with("//") {
        return format!("https:{url}");
    }
    url
}

fn unwrap_yahoo_url(url: &str) -> String {
    let url = html_unescape(url.trim());
    if url.contains("r.search.yahoo.com") {
        if let Some(pos) = url.find("/RU=") {
            let rest = &url[pos + 4..];
            let end = rest.find('/').unwrap_or(rest.len());
            if let Ok(decoded) = url_decode(&rest[..end]) {
                return decoded.to_string();
            }
            return rest[..end].to_string();
        }
    }
    url
}

fn extract_snippet_after(text: &str, marker: &str) -> Option<String> {
    let pos = text.find(marker)?;
    let rest = &text[pos..];
    let open_end = rest.find('>')?;
    let close = rest[open_end + 1..].find("</")?;
    Some(clean_html_text(&rest[open_end + 1..open_end + 1 + close]))
}

fn format_crawler_results(query: &str, provider: &str, results: Vec<CrawlerResult>) -> String {
    let mut lines = vec![
        format!("## Search results for: {query}"),
        format!("**Provider**: {provider}\n"),
    ];
    for (index, r) in results.into_iter().enumerate() {
        lines.push(format!("### {}. {}", index + 1, r.title));
        lines.push(format!("**URL**: {}", r.url));
        lines.push(format!("**Source**: {}", r.source));
        if !r.snippet.is_empty() {
            lines.push(format!("**Snippet**: {}", clip(&r.snippet, 400)));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

// ── DuckDuckGo HTML search ─────────────────────────────────────

async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<String> {
    if is_ddg_blocked() {
        let fallback = search_fallback_html(client, query, max_results).await;
        if !fallback.is_empty() {
            return Ok(format_crawler_results(
                query,
                "DuckDuckGo (via fallback)",
                fallback,
            ));
        }
        bail!("DuckDuckGo is blocked by captcha and fallback engines returned no results");
    }

    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );
    let response = client
        .get(&url)
        .header("User-Agent", CRAWLER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await;

    let html = match response {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            if looks_like_ddg_challenge(status, &text) {
                set_ddg_blocked(Duration::from_secs(60));
                let fallback = search_fallback_html(client, query, max_results).await;
                if !fallback.is_empty() {
                    return Ok(format_crawler_results(
                        query,
                        "DuckDuckGo (via fallback - DDG captcha)",
                        fallback,
                    ));
                }
                bail!(
                    "DuckDuckGo returned a captcha page and fallback engines returned no results"
                );
            }
            if status != 200 {
                let fallback = search_fallback_html(client, query, max_results).await;
                if !fallback.is_empty() {
                    return Ok(format_crawler_results(
                        query,
                        "DuckDuckGo (via fallback - DDG HTTP error)",
                        fallback,
                    ));
                }
                bail!("DuckDuckGo HTTP {status} and fallback returned no results");
            }
            text
        }
        Err(_) => {
            let fallback = search_fallback_html(client, query, max_results).await;
            if !fallback.is_empty() {
                return Ok(format_crawler_results(
                    query,
                    "DuckDuckGo (via fallback - DDG request failed)",
                    fallback,
                ));
            }
            bail!("DuckDuckGo request failed and fallback returned no results");
        }
    };

    let results = parse_duckduckgo_html(&html, max_results);
    if !results.is_empty() {
        return Ok(format_crawler_results(query, "DuckDuckGo HTML", results));
    }

    let fallback = search_fallback_html(client, query, max_results).await;
    if !fallback.is_empty() {
        return Ok(format_crawler_results(
            query,
            "DuckDuckGo (via fallback - DDG no results)",
            fallback,
        ));
    }
    bail!("DuckDuckGo returned no parseable results and fallback returned no results");
}

fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<CrawlerResult> {
    let mut results = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rest = html;
    while let Some(link_pos) = rest.find("result__a") {
        rest = &rest[link_pos..];
        let Some(href_pos) = rest.find("href=\"") else {
            break;
        };
        let href_start = href_pos + "href=\"".len();
        let Some(href_end) = rest[href_start..].find('"') else {
            break;
        };
        let raw_url = unwrap_ddg_url(&rest[href_start..href_start + href_end]);
        let Some(tag_end) = rest[href_start + href_end..].find('>') else {
            break;
        };
        let title_start = href_start + href_end + tag_end + 1;
        let Some(title_end) = rest[title_start..].find("</a>") else {
            break;
        };
        let title = clean_html_text(&rest[title_start..title_start + title_end]);
        let snippet =
            if let Some(snippet_pos) = rest[title_start + title_end..].find("result__snippet") {
                let snippet_rest = &rest[title_start + title_end + snippet_pos..];
                if let Some(open_end) = snippet_rest.find('>') {
                    if let Some(close) = snippet_rest[open_end + 1..].find("</") {
                        clean_html_text(&snippet_rest[open_end + 1..open_end + 1 + close])
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
        if !title.is_empty() && !raw_url.is_empty() && is_result_url_allowed(&raw_url) {
            let key = dedupe_key(&raw_url);
            if seen.insert(key) {
                results.push(CrawlerResult {
                    title,
                    url: raw_url,
                    snippet,
                    source: "DuckDuckGo".to_string(),
                });
            }
        }
        if results.len() >= max_results {
            break;
        }
        rest = &rest[title_start + title_end..];
    }
    results
}

// ── Yahoo HTML search ──────────────────────────────────────────

async fn search_yahoo_html(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    let url = format!(
        "https://search.yahoo.com/search?p={}",
        urlencoding::encode(query)
    );
    let html = match client
        .get(&url)
        .header("User-Agent", CRAWLER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().as_u16() != 200 {
                return Vec::new();
            }
            resp.text().await.unwrap_or_default()
        }
        Err(_) => return Vec::new(),
    };
    parse_yahoo_html(&html, max_results)
}

fn parse_yahoo_html(html: &str, max_results: usize) -> Vec<CrawlerResult> {
    let mut results = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rest = html;
    while let Some(pos) = rest.find("class=\"dd algo") {
        rest = &rest[pos..];
        let anchor_start = match rest.find("href=\"") {
            Some(p) => p + "href=\"".len(),
            None => {
                rest = &rest[10..];
                continue;
            }
        };
        let Some(href_end) = rest[anchor_start..].find('"') else {
            break;
        };
        let raw_url = unwrap_yahoo_url(&rest[anchor_start..anchor_start + href_end]);
        let Some(tag_end) = rest[anchor_start + href_end..].find('>') else {
            rest = &rest[anchor_start + href_end..];
            continue;
        };
        let title_start = anchor_start + href_end + tag_end + 1;
        let Some(title_end) = rest[title_start..].find("</a>") else {
            break;
        };
        let title = clean_html_text(&rest[title_start..title_start + title_end]);
        let snippet = extract_snippet_after(&rest[title_start + title_end..], "compText")
            .or_else(|| extract_snippet_after(&rest[title_start + title_end..], "<p"))
            .unwrap_or_default();
        if !title.is_empty() && !raw_url.is_empty() && is_result_url_allowed(&raw_url) {
            let key = dedupe_key(&raw_url);
            if seen.insert(key) {
                results.push(CrawlerResult {
                    title,
                    url: raw_url,
                    snippet,
                    source: "Yahoo".to_string(),
                });
            }
        }
        if results.len() >= max_results {
            break;
        }
        rest = &rest[title_start + title_end..];
    }
    results
}

// ── 360 (so.com) HTML search ───────────────────────────────────

async fn search_so_html(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    let url = format!("https://www.so.com/s?q={}", urlencoding::encode(query));
    let html = match client
        .get(&url)
        .header("User-Agent", CRAWLER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
    {
        Ok(resp) => {
            if resp.status().as_u16() != 200 {
                return Vec::new();
            }
            resp.text().await.unwrap_or_default()
        }
        Err(_) => return Vec::new(),
    };
    parse_so_html(client, &html, max_results).await
}

async fn parse_so_html(
    client: &reqwest::Client,
    html: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("class=\"result") {
        rest = &rest[pos..];
        let h3_pos = match rest.find("<h3") {
            Some(p) => p,
            None => {
                rest = &rest[10..];
                continue;
            }
        };
        let h3_rest = &rest[h3_pos..];
        let href_start = match h3_rest.find("href=\"") {
            Some(p) => p + "href=\"".len(),
            None => {
                rest = &rest[h3_pos + 3..];
                continue;
            }
        };
        let Some(href_end) = h3_rest[href_start..].find('"') else {
            break;
        };
        let href = html_unescape(&h3_rest[href_start..href_start + href_end]);
        let Some(tag_end) = h3_rest[href_start + href_end..].find('>') else {
            rest = &rest[h3_pos + 3..];
            continue;
        };
        let title_start = href_start + href_end + tag_end + 1;
        let Some(title_end) = h3_rest[title_start..].find("</a>") else {
            break;
        };
        let title = clean_html_text(&h3_rest[title_start..title_start + title_end]);
        let snippet = extract_snippet_after(&h3_rest[title_start + title_end..], "res-desc")
            .or_else(|| extract_snippet_after(&h3_rest[title_start + title_end..], "fz-mid"))
            .or_else(|| extract_snippet_after(&h3_rest[title_start + title_end..], "<p"))
            .unwrap_or_default();
        if !title.is_empty() && !href.is_empty() {
            candidates.push((title, href, snippet));
        }
        if candidates.len() >= max_results * 2 {
            break;
        }
        rest = &h3_rest[title_start + title_end..];
    }

    let mut results = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (title, href, snippet) in candidates {
        if results.len() >= max_results {
            break;
        }
        let resolved = resolve_so_url(client, &href).await;
        if !resolved.is_empty() && is_result_url_allowed(&resolved) {
            let key = dedupe_key(&resolved);
            if seen.insert(key) {
                results.push(CrawlerResult {
                    title,
                    url: resolved,
                    snippet,
                    source: "360".to_string(),
                });
            }
        }
    }
    results
}

async fn resolve_so_url(client: &reqwest::Client, href: &str) -> String {
    let href = html_unescape(href.trim());
    if href.is_empty() {
        return String::new();
    }
    let absolute = if href.starts_with("http://") || href.starts_with("https://") {
        href.clone()
    } else {
        format!("https://www.so.com{}", href)
    };
    if !(absolute.contains("so.com") && absolute.contains("/link")) {
        return absolute;
    }
    match client.get(&absolute).send().await {
        Ok(resp) => {
            let final_url = resp.url().to_string();
            if final_url != absolute
                && (final_url.starts_with("http://") || final_url.starts_with("https://"))
            {
                return final_url;
            }
            let text = resp.text().await.unwrap_or_default();
            if let Some(pos) = text.find("window.location") {
                let rest = &text[pos..];
                if let Some(q1) = rest.find('"') {
                    if let Some(q2) = rest[q1 + 1..].find('"') {
                        return html_unescape(&rest[q1 + 1..q1 + 1 + q2]);
                    }
                }
            }
            if let Some(pos) = text.find("URL=") {
                let rest = &text[pos + 4..];
                let end = rest
                    .find('"')
                    .or_else(|| rest.find('>'))
                    .unwrap_or(rest.len());
                let url_str = rest[..end].trim_matches('\'');
                return html_unescape(url_str);
            }
            absolute
        }
        Err(_) => absolute,
    }
}

// ── Sogou HTML search ──────────────────────────────────────────

async fn search_sogou_html(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    if is_sogou_blocked() {
        return Vec::new();
    }
    let url = format!(
        "https://www.sogou.com/web?query={}&ie=utf8",
        urlencoding::encode(query)
    );
    let html = match client
        .get(&url)
        .header("User-Agent", CRAWLER_USER_AGENT)
        .header(
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .send()
        .await
    {
        Ok(resp) => {
            let final_url = resp.url().to_string();
            let text = resp.text().await.unwrap_or_default();
            if final_url.contains("antispider")
                || text.contains("SourceVerifyCode")
                || text.contains("\u{6b64}\u{9a8c}\u{8bc1}\u{7801}\u{7528}\u{4e8e}\u{786e}\u{8ba4}")
            {
                set_sogou_blocked(Duration::from_secs(300));
                return Vec::new();
            }
            text
        }
        Err(_) => return Vec::new(),
    };
    parse_sogou_html(client, &html, max_results).await
}

async fn parse_sogou_html(
    client: &reqwest::Client,
    html: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("class=\"vrwrap") {
        rest = &rest[pos..];
        let h3_pos = match rest.find("<h3") {
            Some(p) => p,
            None => {
                rest = &rest[10..];
                continue;
            }
        };
        let h3_rest = &rest[h3_pos..];
        let href_start = match h3_rest.find("href=\"") {
            Some(p) => p + "href=\"".len(),
            None => {
                rest = &rest[h3_pos + 3..];
                continue;
            }
        };
        let Some(href_end) = h3_rest[href_start..].find('"') else {
            break;
        };
        let href = html_unescape(&h3_rest[href_start..href_start + href_end]);
        let Some(tag_end) = h3_rest[href_start + href_end..].find('>') else {
            rest = &rest[h3_pos + 3..];
            continue;
        };
        let title_start = href_start + href_end + tag_end + 1;
        let Some(title_end) = h3_rest[title_start..].find("</a>") else {
            break;
        };
        let title = clean_html_text(&h3_rest[title_start..title_start + title_end]);
        let snippet = extract_snippet_after(&h3_rest[title_start + title_end..], "fz-mid")
            .or_else(|| extract_snippet_after(&h3_rest[title_start + title_end..], "str_info"))
            .or_else(|| extract_snippet_after(&h3_rest[title_start + title_end..], "<p"))
            .unwrap_or_default();
        if !title.is_empty() && !href.is_empty() {
            candidates.push((title, href, snippet));
        }
        if candidates.len() >= max_results * 2 {
            break;
        }
        rest = &h3_rest[title_start + title_end..];
    }

    let mut results = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (title, href, snippet) in candidates {
        if results.len() >= max_results {
            break;
        }
        let resolved = resolve_sogou_url(client, &href).await;
        if !resolved.is_empty() && is_result_url_allowed(&resolved) {
            let key = dedupe_key(&resolved);
            if seen.insert(key) {
                results.push(CrawlerResult {
                    title,
                    url: resolved,
                    snippet,
                    source: "Sogou".to_string(),
                });
            }
        }
    }
    results
}

async fn resolve_sogou_url(client: &reqwest::Client, href: &str) -> String {
    let href = html_unescape(href.trim());
    if href.is_empty() {
        return String::new();
    }
    let absolute = if href.starts_with("http://") || href.starts_with("https://") {
        href.clone()
    } else {
        format!("https://www.sogou.com{}", href)
    };
    if !(absolute.contains("sogou.com") && absolute.contains("/link")) {
        return absolute;
    }
    match client.get(&absolute).send().await {
        Ok(resp) => {
            let final_url = resp.url().to_string();
            if final_url != absolute
                && (final_url.starts_with("http://") || final_url.starts_with("https://"))
            {
                return final_url;
            }
            let text = resp.text().await.unwrap_or_default();
            if let Some(pos) = text.find("window.location") {
                let rest = &text[pos..];
                if let Some(q1) = rest.find('"') {
                    if let Some(q2) = rest[q1 + 1..].find('"') {
                        return html_unescape(&rest[q1 + 1..q1 + 1 + q2]);
                    }
                }
            }
            if let Some(pos) = text.find("URL=") {
                let rest = &text[pos + 4..];
                let end = rest
                    .find('"')
                    .or_else(|| rest.find('>'))
                    .unwrap_or(rest.len());
                let url_str = rest[..end].trim_matches('\'');
                return html_unescape(url_str);
            }
            absolute
        }
        Err(_) => absolute,
    }
}

// ── Multi-engine fallback dispatcher ────────────────────────────

async fn search_fallback_html(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Vec<CrawlerResult> {
    let yahoo_results = search_yahoo_html(client, query, max_results).await;
    if yahoo_results.len() >= max_results.min(5) {
        return yahoo_results;
    }

    let mut combined: Vec<CrawlerResult> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in yahoo_results {
        let key = dedupe_key(&r.url);
        if seen.insert(key) {
            combined.push(r);
        }
    }

    let so_results = search_so_html(client, query, max_results).await;
    for r in so_results {
        if combined.len() >= max_results {
            break;
        }
        let key = dedupe_key(&r.url);
        if seen.insert(key) {
            combined.push(r);
        }
    }

    if combined.len() < max_results {
        let sogou_results = search_sogou_html(client, query, max_results).await;
        for r in sogou_results {
            if combined.len() >= max_results {
                break;
            }
            let key = dedupe_key(&r.url);
            if seen.insert(key) {
                combined.push(r);
            }
        }
    }

    combined
}

fn clean_html_text(value: &str) -> String {
    html_unescape(&html2text::from_read(value.as_bytes(), 120))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_unescape(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn format_search_results(query: &str, provider: &str, results: Vec<Value>) -> String {
    let mut lines = vec![
        format!("## Search results for: {query}"),
        format!("**Provider**: {provider}\n"),
    ];
    for (index, item) in results.into_iter().enumerate() {
        let title = item
            .get("title")
            .or_else(|| item.pointer("/metadata/title"))
            .and_then(Value::as_str)
            .unwrap_or("Untitled");
        let url = item
            .get("url")
            .or_else(|| item.pointer("/metadata/sourceURL"))
            .or_else(|| item.pointer("/metadata/url"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let snippet = item
            .get("content")
            .or_else(|| item.get("snippet"))
            .or_else(|| item.get("description"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let raw = item
            .get("raw_content")
            .or_else(|| item.get("markdown"))
            .and_then(Value::as_str)
            .unwrap_or("");
        lines.push(format!("### {}. {title}", index + 1));
        if !url.is_empty() {
            lines.push(format!("**URL**: {url}"));
        }
        if !snippet.is_empty() {
            lines.push(format!("**Snippet**: {}", clip(snippet, 500)));
        }
        if !raw.is_empty() {
            lines.push(format!("**Content**: {}", clip(raw, 800)));
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn clip(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!("{}...", value.chars().take(max_chars).collect::<String>())
    }
}

async fn web_fetch(args: Value) -> Result<String> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("URL must start with http:// or https://");
    }
    let format = args
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or("markdown");
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .min(120);
    let max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .map(|value| value.clamp(1, MAX_FETCH_CHARS as u64) as usize)
        .unwrap_or(DEFAULT_FETCH_MAX_CHARS);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout))
        .build()?;
    let accept = match format {
        "text" => "text/plain;q=1.0, text/markdown;q=0.9, text/html;q=0.8, */*;q=0.1",
        "html" => "text/html;q=1.0, application/xhtml+xml;q=0.9, text/plain;q=0.8, */*;q=0.1",
        _ => "text/markdown;q=1.0, text/x-markdown;q=0.9, text/plain;q=0.8, text/html;q=0.7, */*;q=0.1",
    };
    let response = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .header("Accept", accept)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?;
    if response.content_length().unwrap_or(0) > MAX_RESPONSE_SIZE as u64 {
        bail!("response too large (exceeds 5MB limit)");
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_RESPONSE_SIZE {
        bail!("response too large (exceeds 5MB limit)");
    }
    let content = String::from_utf8_lossy(&bytes).to_string();
    let output = if content_type.contains("text/html") {
        match format {
            "html" => content,
            "text" => html2text::from_read(content.as_bytes(), 120),
            _ => html2md::parse_html(&content),
        }
    } else {
        content
    };
    Ok(clip_fetch_output(&output, max_chars))
}

fn clip_fetch_output(value: &str, max_chars: usize) -> String {
    let total = value.chars().count();
    if total <= max_chars {
        return value.to_string();
    }
    let clipped = value.chars().take(max_chars).collect::<String>();
    format!("{clipped}\n\n[content truncated from {total} chars to {max_chars} chars]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_fetch_output_with_notice() {
        let output = clip_fetch_output("abcdef", 3);

        assert_eq!(output, "abc\n\n[content truncated from 6 chars to 3 chars]");
    }

    #[test]
    fn keeps_short_fetch_output_unchanged() {
        assert_eq!(clip_fetch_output("abc", 3), "abc");
    }
}
