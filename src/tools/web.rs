use super::{ToolRegistry, ToolSpec};
use crate::config::WebPluginConfig;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const MAX_RESPONSE_SIZE: usize = 5 * 1024 * 1024;

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
                "timeout": { "type": "integer", "description": "Timeout seconds, max 120." }
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
        "Search the web. Prefer configured Tavily, Firecrawl, or AnySearch API keys; fallback to SearXNG, then built-in DuckDuckGo HTML search when providers fail.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "max_results": { "type": "integer", "description": "Maximum results, default 5." },
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
        .unwrap_or(5)
        .clamp(1, 10) as usize;
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("auto");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
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
    bail!("no web search provider succeeded; API keys missing/failed, SearXNG unavailable, and built-in DuckDuckGo fallback returned no results")
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

async fn search_duckduckgo(
    client: &reqwest::Client,
    query: &str,
    max_results: usize,
) -> Result<String> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );
    let html = client
        .get(url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let results = parse_duckduckgo_html(&html, max_results);
    if results.is_empty() {
        bail!("DuckDuckGo returned no parseable results");
    }
    let mut lines = vec![
        format!("## Search results for: {query}"),
        "**Provider**: DuckDuckGo HTML fallback\n".to_string(),
    ];
    for (index, (title, url, snippet)) in results.into_iter().enumerate() {
        lines.push(format!("### {}. {title}", index + 1));
        lines.push(format!("**URL**: {url}"));
        if !snippet.is_empty() {
            lines.push(format!("**Snippet**: {snippet}"));
        }
        lines.push(String::new());
    }
    Ok(lines.join("\n"))
}

fn parse_duckduckgo_html(html: &str, max_results: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
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
        let raw_url = html_unescape(&rest[href_start..href_start + href_end]);
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
        if !title.is_empty() && !raw_url.is_empty() {
            results.push((title, raw_url, snippet));
        }
        if results.len() >= max_results {
            break;
        }
        rest = &rest[title_start + title_end..];
    }
    results
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
    if content_type.contains("text/html") {
        return Ok(match format {
            "html" => content,
            "text" => html2text::from_read(content.as_bytes(), 120),
            _ => html2md::parse_html(&content),
        });
    }
    Ok(content)
}
