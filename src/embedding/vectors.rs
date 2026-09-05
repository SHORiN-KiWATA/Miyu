//! 向量的存储与融合小工具。
//!
//! 向量一律以小端 f32 BLOB 落库：1 万条 1024 维 JSON 文本是 136 MB、每次查询要
//! 解析 2 秒，同样的数据 BLOB 只有 43 MB 且点积毫秒级（09-05 实测）。

use std::collections::HashMap;
use std::hash::Hash;

pub(crate) fn cosine(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm.sqrt() * right_norm.sqrt())
    }
}

pub(crate) fn vector_to_blob(vector: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

pub(crate) fn vector_from_blob(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.is_empty() || blob.len() % 4 != 0 {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

/// Reciprocal-rank fusion. Each ranking is best-first; an item's score is the
/// sum of `1 / (k + rank)` over the rankings it appears in. Scale-free, so a
/// keyword scorer in the tens and a cosine in the tenths fuse without tuning
/// (09-05 实测:记忆 hit@3 关键词 51% / 语义 61% / RRF 69%)。
pub(crate) fn rrf_fuse<K: Eq + Hash + Clone>(rankings: &[Vec<K>], k: f64) -> Vec<(K, f64)> {
    let mut scores: HashMap<K, f64> = HashMap::new();
    let mut order: Vec<K> = Vec::new();
    for ranking in rankings {
        for (rank, key) in ranking.iter().enumerate() {
            let entry = scores.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                0.0
            });
            *entry += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(K, f64)> = order
        .into_iter()
        .map(|key| {
            let score = scores[&key];
            (key, score)
        })
        .collect();
    // Stable sort keeps first-seen order for ties, so the keyword ranking
    // (passed first) wins ties against the semantic one.
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused
}

pub(crate) const RRF_K: f64 = 60.0;
