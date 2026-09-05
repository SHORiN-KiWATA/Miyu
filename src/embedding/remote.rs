//! 远程 OpenAI 兼容 `/embeddings` 后端（原 `knowledge_base::embed_text`）。

use crate::config::ProviderConfig;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

pub(crate) async fn embed_remote(
    provider: &ProviderConfig,
    model: &str,
    timeout: Duration,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let api_key = provider.api_key.as_deref().unwrap_or_default().trim();
    if api_key.is_empty() {
        bail!("embedding provider {} has no api_key", provider.id)
    }
    let client = Client::builder()
        .timeout(timeout.max(Duration::from_secs(1)))
        .build()?;
    let url = format!("{}/embeddings", provider.base_url.trim_end_matches('/'));
    let input: Value = if texts.len() == 1 {
        json!(texts[0])
    } else {
        json!(texts)
    };
    let response = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&json!({ "model": model, "input": input }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = text.chars().take(400).collect::<String>();
        bail!("embedding API error at {url} ({status}): {text}");
    }
    let data: Value = response.json().await?;
    let items = data
        .get("data")
        .and_then(Value::as_array)
        .context("embedding response missing data[]")?;
    let mut ordered: Vec<(usize, Vec<f32>)> = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(position);
        let vector = item
            .get("embedding")
            .and_then(Value::as_array)
            .context("embedding response item missing embedding[]")?
            .iter()
            .filter_map(Value::as_f64)
            .map(|value| value as f32)
            .collect::<Vec<_>>();
        ordered.push((index, vector));
    }
    ordered.sort_by_key(|(index, _)| *index);
    if ordered.len() != texts.len() {
        bail!(
            "embedding response returned {} vectors for {} inputs",
            ordered.len(),
            texts.len()
        );
    }
    Ok(ordered.into_iter().map(|(_, vector)| vector).collect())
}
