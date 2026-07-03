use super::{ToolRegistry, ToolSpec};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

const PROTONDB_BASE: &str = "https://www.protondb.com";
const ALGOLIA_URL: &str = "https://94he6yatei-dsn.algolia.net/1/indexes/steamdb/query";

const TOOL_NAME: &str = "protondb_query";
const TOOL_DESC: &str = "Query ProtonDB game compatibility ratings and user reports. Use when you need to check Linux game compatibility, ProtonDB tiers, or read user reports/comments for a game. Accepts either a Steam App ID (numeric) or a game title (text search). / 查询 ProtonDB 游戏兼容性评级和用户评论。在需要查询 Linux 游戏兼容性信息等场景时使用。支持 Steam App ID（数字）或游戏名称（文本搜索）。";

pub fn register(registry: &mut ToolRegistry) {
    registry.register(create_toolspec());
}

pub fn create_toolspec() -> ToolSpec {
    ToolSpec::new(
        TOOL_NAME,
        TOOL_DESC,
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Steam App ID (e.g. \"1245620\") or game title (e.g. \"Elden Ring\"). / Steam App ID 或游戏名称。"
                },
                "max_reports": {
                    "type": "integer",
                    "description": "Maximum number of user reports to return, default 10. / 最多返回的评论数，默认 10。"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        |args| async move { protondb_query(args).await },
    )
}

async fn protondb_query(args: Value) -> Result<String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        bail!("missing required argument: query");
    }
    let max_reports = args
        .get("max_reports")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(40) as usize;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("miyu-protondb-query/0.1")
        .build()?;

    // Resolve app_id: numeric → direct, otherwise search via Algolia
    let (app_id, game_name, oslist) = if query.chars().all(|c| c.is_ascii_digit()) && !query.is_empty() {
        let id: u64 = query.parse()?;
        let (name, os) = search_game_info(&client, query).await
            .unwrap_or((query.to_string(), Vec::new()));
        (id, name, os)
    } else {
        let (id, name, os) = search_game(&client, query).await?;
        (id, name, os)
    };

    // Fetch summary
    let summary = fetch_json(
        &client,
        &format!("{PROTONDB_BASE}/api/v1/reports/summaries/{app_id}.json"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("ProtonDB summary fetch failed for app {app_id}: {e}"))?;

    // Fetch reports
    let reports = fetch_reports(&client, app_id).await.unwrap_or_else(|e| {
        json!({
            "error": e.to_string(),
            "reports": [],
            "total": 0,
        })
    });

    let total_reports = reports["total"].as_u64().unwrap_or(0);
    let raw_reports = reports["reports"].as_array().cloned().unwrap_or_default();
    let extracted: Vec<Value> = raw_reports
        .iter()
        .take(max_reports)
        .map(|r| extract_report(r))
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "app_id": app_id,
        "game_name": game_name,
        "oslist": oslist,
        "summary": {
            "tier": summary["tier"],
            "confidence": summary["confidence"],
            "score": summary["score"],
            "total": summary["total"],
            "best_reported_tier": summary["bestReportedTier"],
            "trending_tier": summary["trendingTier"],
        },
        "reports": {
            "total": total_reports,
            "returned": extracted.len(),
            "items": extracted,
        },
        "protondb_url": format!("https://www.protondb.com/app/{app_id}"),
    }))?)
}

// ── Algolia search ───────────────────────────────────

async fn search_game(client: &reqwest::Client, query: &str) -> Result<(u64, String, Vec<String>)> {
    let body = json!({
        "query": query,
        "facetFilters": [["appType:Game"]],
        "hitsPerPage": 1,
        "attributesToRetrieve": ["name", "objectID", "oslist"],
        "page": 0,
    });
    let resp: Value = client
        .post(ALGOLIA_URL)
        .header("x-algolia-api-key", "9ba0e69fb2974316cdaec8f5f257088f")
        .header("x-algolia-application-id", "94HE6YATEI")
        .header("Referer", "https://www.protondb.com")
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let hit = resp["hits"]
        .as_array()
        .and_then(|hits| hits.first())
        .ok_or_else(|| anyhow::anyhow!("no search results for \"{query}\""))?;

    let app_id: u64 = hit["objectID"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid objectID in search result"))?;
    let name = hit["name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let oslist = hit["oslist"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok((app_id, name, oslist))
}

/// When user provides a numeric app_id directly, try to look up the game name via search.
async fn search_game_info(
    client: &reqwest::Client,
    app_id: &str,
) -> Option<(String, Vec<String>)> {
    let body = json!({
        "query": app_id,
        "facetFilters": [["appType:Game"]],
        "hitsPerPage": 1,
        "attributesToRetrieve": ["name", "objectID", "oslist"],
        "page": 0,
    });
    let resp: Value = client
        .post(ALGOLIA_URL)
        .header("x-algolia-api-key", "9ba0e69fb2974316cdaec8f5f257088f")
        .header("x-algolia-application-id", "94HE6YATEI")
        .header("Referer", "https://www.protondb.com")
        .json(&body)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;

    let hit = resp["hits"].as_array()?.first()?;
    if hit["objectID"].as_str() == Some(app_id) {
        let name = hit["name"].as_str().unwrap_or(app_id).to_string();
        let oslist = hit["oslist"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        Some((name, oslist))
    } else {
        None
    }
}

// ── Reports fetch (hash-based URL) ───────────────────

async fn fetch_reports(client: &reqwest::Client, app_id: u64) -> Result<Value> {
    let counts = fetch_json(client, &format!("{PROTONDB_BASE}/data/counts.json")).await?;
    let reports_count = counts["reports"].as_u64().unwrap_or(0);
    let timestamp = counts["timestamp"].as_u64().unwrap_or(0);
    if reports_count == 0 || timestamp == 0 {
        bail!("invalid counts.json");
    }
    let pid = calculate_protondb_id(app_id, reports_count, timestamp);
    let url = format!("{PROTONDB_BASE}/data/reports/all-devices/app/{pid}.json");
    fetch_json(client, &url).await
}

/// Hash computation — reverse-engineered from ProtonDB frontend JS.
/// R(e, t, n) = t + "p" + (e * (t % n))
fn hash_r(e: u64, t: u64, n: u64) -> String {
    format!("{}p{}", t, e.wrapping_mul(t % n))
}

/// I(s) = abs(foldl(s + "m", |0, (acc, ch) => ((acc << 5) - acc + charCode) | 0))
/// JS `| 0` truncates to signed 32-bit; we replicate with i32 wrapping arithmetic.
fn hash_i(s: &str) -> u32 {
    let mut val: i32 = 0;
    for ch in s.chars().chain(['m']) {
        val = val
            .wrapping_shl(5)
            .wrapping_sub(val)
            .wrapping_add(ch as i32);
    }
    val.abs() as u32
}

fn calculate_protondb_id(steam_id: u64, reports_count: u64, timestamp: u64) -> u32 {
    let h1 = hash_r(steam_id, reports_count, timestamp);
    let h2 = hash_r(1, steam_id, timestamp);
    let h3 = format!("p{h1}*vRT{h2}undefined");
    hash_i(&h3)
}

// ── Report extraction ────────────────────────────────

fn extract_report(r: &Value) -> Value {
    let contributor = &r["contributor"]["steam"];
    let nickname = contributor["nickname"].as_str().unwrap_or("anonymous");
    let report_tally = contributor["reportTally"].as_u64().unwrap_or(0);
    let playtime = contributor["playtime"].as_u64().unwrap_or(0);
    let playtime_hours = if playtime > 0 { Some(playtime / 60) } else { None };

    let timestamp = r["timestamp"].as_u64().unwrap_or(0);
    let date = format_date(timestamp);

    let responses = &r["responses"];
    let notes = &responses["notes"];

    let starts_play = responses["startsPlay"].as_str().unwrap_or("no");
    let verdict = responses["verdict"].as_str();
    let verdict_oob = responses["verdictOob"].as_str();
    let variant = responses["variant"].as_str().unwrap_or("");
    let custom_proton = responses["customProtonVersion"].as_str();
    let proton_version = responses["protonVersion"].as_str();
    let launch_options = responses["launchOptions"].as_str();
    let concluding = notes["concludingNotes"]
        .as_str()
        .or_else(|| responses["concludingNotes"].as_str());

    // Determine recommendation status
    let recommended = if starts_play != "yes" {
        "broken"
    } else if let Some(v) = verdict_oob {
        if v == "yes" { "recommended" } else { "not_recommended" }
    } else if let Some(v) = verdict {
        if v == "yes" { "recommended" } else { "not_recommended" }
    } else {
        "unknown"
    };

    // Proton version info
    let proton_info = if variant == "experimental" {
        Some("Proton Experimental".to_string())
    } else if variant == "ge" {
        custom_proton.map(|s| s.to_string())
    } else if variant == "notListed" {
        proton_version.map(|s| s.to_string())
    } else {
        proton_version.map(|s| s.to_string())
    };

    // Extract faults
    let faults = extract_faults(responses, notes);

    // Extract launch options note
    let launch_opts = if let Some(lo) = launch_options {
        let trimmed = lo.trim();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    } else {
        None
    };

    json!({
        "author": nickname,
        "report_count": report_tally,
        "playtime_hours": playtime_hours,
        "date": date,
        "recommended": recommended,
        "proton_version": proton_info,
        "launch_options": launch_opts,
        "faults": faults,
        "notes": concluding.and_then(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }),
    })
}

fn extract_faults(responses: &Value, notes: &Value) -> Vec<Value> {
    let follow_up = &responses["followUp"];
    let fault_types = [
        ("audioFaults", "audio"),
        ("graphicalFaults", "graphics"),
        ("windowingFaults", "windowing"),
        ("inputFaults", "input"),
        ("saveGameFaults", "save_game"),
        ("performanceFaults", "performance"),
        ("stabilityFaults", "stability"),
        ("significantBugs", "significant_bugs"),
    ];

    fault_types
        .iter()
        .filter_map(|(key, label)| {
            if responses[key].as_str() != Some("yes") {
                return None;
            }

            // follow_up[key] can be a dict (of sub-faults) or a string
            let details: Vec<String> = if let Some(fu) = follow_up.get(key) {
                if let Some(obj) = fu.as_object() {
                    obj.keys().cloned().collect()
                } else if let Some(s) = fu.as_str() {
                    vec![s.to_string()]
                } else {
                    vec![]
                }
            } else {
                vec![]
            };

            let note = notes[key].as_str().and_then(|s| {
                let trimmed = s.trim();
                if trimmed.is_empty() { None } else { Some(trimmed) }
            });

            Some(json!({
                "type": label,
                "details": details,
                "note": note,
            }))
        })
        .collect()
}

// ── Utilities ────────────────────────────────────────

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

fn format_date(timestamp: u64) -> String {
    // Simple date formatting without chrono dependency
    // Unix timestamp → YYYY-MM-DD
    if timestamp == 0 {
        return "unknown".to_string();
    }
    let secs = timestamp as i64;
    let days = secs / 86400;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert days since Unix epoch (1970-01-01) to (year, month, day).
fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    // Algorithm from Howard Hinnant's date library (civil_from_days)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_r() {
        assert_eq!(hash_r(1, 2, 3), "2p2");
    }

    #[test]
    fn test_hash_i_basic() {
        // Verify deterministic output
        let result = hash_i("test");
        assert!(result > 0);
    }

    #[test]
    fn test_days_to_ymd() {
        // Unix epoch
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        // 2025-01-01
        assert_eq!(days_to_ymd(20089), (2025, 1, 1));
        // 2024-02-29 (leap year)
        assert_eq!(days_to_ymd(19782), (2024, 2, 29));
    }

    #[test]
    fn test_format_date() {
        assert_eq!(format_date(0), "unknown");
        // 2025-01-01 00:00:00 UTC = 1735689600
        assert_eq!(format_date(1735689600), "2025-01-01");
    }

    #[test]
    fn test_extract_report_basic() {
        let report = json!({
            "contributor": {
                "steam": {
                    "nickname": "test_user",
                    "reportTally": 5,
                    "playtime": 600
                }
            },
            "timestamp": 1735689600,
            "responses": {
                "startsPlay": "yes",
                "verdict": "yes",
                "variant": "ge",
                "customProtonVersion": "GE-Proton9-4",
                "notes": {
                    "concludingNotes": "Works great!"
                }
            }
        });
        let extracted = extract_report(&report);
        assert_eq!(extracted["author"], "test_user");
        assert_eq!(extracted["playtime_hours"], 10);
        assert_eq!(extracted["recommended"], "recommended");
        assert_eq!(extracted["proton_version"], "GE-Proton9-4");
        assert_eq!(extracted["notes"], "Works great!");
    }

    #[test]
    fn test_extract_faults_with_dict_followup() {
        let responses = json!({
            "audioFaults": "yes",
            "followUp": {
                "audioFaults": {
                    "crackling": true,
                    "desync": true
                }
            }
        });
        let notes = json!({});
        let faults = extract_faults(&responses, &notes);
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0]["type"], "audio");
        assert!(faults[0]["details"].is_array());
    }

    #[test]
    fn test_extract_faults_with_string_followup() {
        let responses = json!({
            "graphicalFaults": "yes",
            "followUp": {
                "graphicalFaults": "minorArtifacts"
            }
        });
        let notes = json!({
            "graphicalFaults": "some flickering"
        });
        let faults = extract_faults(&responses, &notes);
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0]["type"], "graphics");
        assert_eq!(faults[0]["details"][0], "minorArtifacts");
        assert_eq!(faults[0]["note"], "some flickering");
    }

    #[test]
    fn test_broken_game() {
        let report = json!({
            "contributor": {"steam": {"nickname": "user"}},
            "timestamp": 1735689600,
            "responses": {
                "startsPlay": "no",
                "installs": "no"
            }
        });
        let extracted = extract_report(&report);
        assert_eq!(extracted["recommended"], "broken");
    }
}
