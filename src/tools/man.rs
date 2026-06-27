use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};

const ARCH_BASE: &str = "https://man.archlinux.org";
const MAN7_BASE: &str = "https://man7.org/linux/man-pages";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "online_man_search",
        "Search online Linux man pages using Arch manual pages.",
        json!({"type":"object","properties":{"query":{"type":"string"},"section":{"type":"string"},"language":{"type":"string"},"limit":{"type":"integer"}},"required":["query"],"additionalProperties":false}),
        |args| async move { search(args).await },
    ));
    registry.register(ToolSpec::new(
        "online_man_get_page",
        "Fetch an online Linux man page from Arch man pages or man7.org.",
        json!({"type":"object","properties":{"name":{"type":"string"},"section":{"type":"string"},"source":{"type":"string","enum":["auto","arch","man7"]},"language":{"type":"string"},"max_chars":{"type":"integer","description":"Maximum returned characters. Use at least 8000 for normal reading; omit unless user asks for a short excerpt."}},"required":["name"],"additionalProperties":false}),
        |args| async move { get_page(args).await },
    ));
}

async fn search(args: Value) -> Result<String> {
    let query = required(&args, "query")?;
    let section = args
        .get("section")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let language = args
        .get("language")
        .and_then(Value::as_str)
        .unwrap_or("en")
        .trim();
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(50) as usize;
    let mut url = format!(
        "{ARCH_BASE}/search?q={}&lang={}",
        urlencoding::encode(&query),
        urlencoding::encode(language)
    );
    if !section.is_empty() {
        url.push_str(&format!("&section={}", urlencoding::encode(section)));
    }
    let html = reqwest::get(url).await?.error_for_status()?.text().await?;
    let mut results = Vec::new();
    for line in html.lines() {
        if let Some(pos) = line.find("/man/") {
            let tail = &line[pos + 5..];
            if let Some(end) = tail.find('.').or_else(|| tail.find('"')) {
                let name = tail[..end].trim_matches('/');
                if !name.is_empty() && !results.iter().any(|item: &String| item.contains(name)) {
                    results.push(format!("- {name}: {ARCH_BASE}/man/{tail}"));
                }
            }
        }
        if results.len() >= limit {
            break;
        }
    }
    if results.is_empty() {
        Ok(format!("No man page search results for {query}"))
    } else {
        Ok(results.join("\n"))
    }
}

async fn get_page(args: Value) -> Result<String> {
    let name = required(&args, "name")?;
    let section = args
        .get("section")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let source = args.get("source").and_then(Value::as_str).unwrap_or("auto");
    let language = args.get("language").and_then(Value::as_str).unwrap_or("en");
    let max_chars = args
        .get("max_chars")
        .and_then(Value::as_u64)
        .unwrap_or(16_000)
        .clamp(2_000, 100_000) as usize;
    let sections: Vec<&str> = if section.is_empty() {
        vec!["1", "8", "5", "7", "2", "3", "4", "6"]
    } else {
        vec![section]
    };
    let try_arch = source == "auto" || source == "arch";
    let try_man7 = source == "auto" || source == "man7";
    if try_arch {
        for sec in &sections {
            let url = format!("{ARCH_BASE}/man/{name}.{sec}.{language}.txt");
            if let Ok(text) = fetch_text(&url).await {
                return Ok(clip(&format!("Source: {url}\n\n{text}"), max_chars));
            }
        }
    }
    if try_man7 {
        for sec in &sections {
            let url = format!("{MAN7_BASE}/man{}/{name}.{sec}.html", &sec[..1]);
            if let Ok(html) = fetch_text(&url).await {
                let text = html2text::from_read(html.as_bytes(), 120);
                return Ok(clip(&format!("Source: {url}\n\n{text}"), max_chars));
            }
        }
    }
    bail!("man page not found: {name}")
}

async fn fetch_text(url: &str) -> Result<String> {
    Ok(reqwest::get(url).await?.error_for_status()?.text().await?)
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

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!(
            "{}\n...[truncated]",
            text.chars().take(max_chars).collect::<String>()
        )
    }
}
