//! 整理器的语义去重：模型说「新建」，库里却已经有换个说法的同一条时，改写成
//! 对已有事实的 update。
//!
//! 词面去重（`apply_organized_batch` 里那条 `NOT EXISTS`）只认字节完全相同的
//! 行，换个说法、加个日期前缀、少一个标点就穿过去了。09-10 真实库取证：GLM
//! 接入存了两条近义，compact 自检存了两条几乎相同的长期日记；候选列表是按
//! jieba 词面重叠挑的，换个说法的旧事实分数为零，模型看不见就只能再建一条。
//!
//! 两件事，都不越过落库校验：
//! 1. [`MemoryStore::widen_existing_candidates`] 把语义相近的已有事实补进候选
//!    列表，让模型有机会自己选 update；
//! 2. [`MemoryStore::fold_near_duplicate_creates`] 兜住模型仍然发出的 create：
//!    近义命中改成 update。改写只在**能证明合法**时落地（同一套 validate），
//!    拿不准就原样保留 create——多一条近义事实也比丢掉一条事实好。
//!
//! 嵌入不可用时退回归一化文本比较（大小写、空白、标点、全半角），这是纯词面
//! 的近义下限。任何一步失败都只是不折叠，不影响整理本身。

use crate::embedding::{cosine, vector_from_blob, vector_to_blob, Embedder};
use crate::memory::*;
use sha2::{Digest, Sha256};

/// 两条事实算「同一条」的余弦下限。检索地板（`min_score`，默认 0.35）对去重
/// 太松：它只保证「相关」，这里要的是「同一件事换个说法」。
///
/// 0.88 是拿本机 bge-small-zh-int8 实测定的：无关内容 0.38，同一件事换个说法
/// 0.88~0.93，而「同一主题但结论相反」也在 0.91 上下——后一档折叠成 update
/// 正是提示词要的（矛盾用 update 改写而不是并存），旧内容留在 memory_revisions
/// 里可查，所以宁可折得动一些。
const NEAR_DUPLICATE_COSINE: f32 = 0.88;
/// 补进候选列表的语义候选上限（词面候选另算，见 `MAX_ORGANIZED_ITEMS`）。
const WIDEN_LIMIT: usize = 8;
/// 单次整理最多现算多少个向量；库里已有存量的行不占额度。
const MAX_FRESH_EMBEDDINGS: usize = 64;
/// 去重扫描的库规模上限，与语义语料同量级：最近这么多行足够，整理是周期任务。
const DEDUP_CORPUS_LIMIT: usize = 500;
/// 检索用的批次文本上限。嵌入模型只认前几百 token，拼一整批日记再截断纯属
/// 浪费，取开头这一段就够定位主题。
const QUERY_CHARS: usize = 1_500;

/// 一行已有事实，连同它的存量向量（内容 sha 对得上才算数）。
struct DedupRow {
    id: i64,
    content: String,
    truth_status: String,
    visibility: String,
    owner_principal: String,
    owner_display_name: String,
    vector: Option<Vec<f32>>,
}

impl DedupRow {
    fn as_existing(&self) -> ExistingMemoryRecord {
        ExistingMemoryRecord {
            id: self.id,
            kind: "knowledge".to_string(),
            content: self.content.clone(),
            truth_status: self.truth_status.clone(),
            visibility: self.visibility.clone(),
            owner_principal: self.owner_principal.clone(),
            owner_display_name: self.owner_display_name.clone(),
        }
    }
}

impl MemoryStore {
    /// 把语义相近的已有事实补进候选列表，返回补进去的条数。
    ///
    /// 补进来的行同时成为合法的 update 目标（validate 只认这份名单），所以这
    /// 一步是「模型自己选 update」的前提，也给 `fold_near_duplicate_creates`
    /// 提供了更大的目标池。尽力而为：没有嵌入器、向量算不出来、库里没有相近
    /// 的，都只是不补。
    pub(crate) async fn widen_existing_candidates(&self, batch: &mut OrganizationBatch) -> usize {
        let Some(embedder) = Embedder::from_config(&self.app_config) else {
            return 0;
        };
        let query = candidate_query_text(&batch.diaries);
        if query.trim().is_empty() {
            return 0;
        }
        // 一批日记拼起来可能上万字,嵌入模型只认前几百 token;取开头一段就够
        // 定位主题,剩下的截掉,别让整批的白算。
        let query = query.chars().take(QUERY_CHARS).collect::<String>();
        let query_vector = match embedder.embed_query(&query).await {
            Ok(vector) => vector,
            Err(error) => {
                tracing::warn!(error = %error, "{}", crate::i18n::text("memory candidate widening skipped: embedding failed", "记忆候选扩展跳过:向量计算失败"));
                return 0;
            }
        };
        let (allowed_principals, privileged_source) = candidate_visibility_scope(&batch.diaries);
        let known = batch
            .existing
            .iter()
            .map(|memory| memory.id)
            .collect::<BTreeSet<_>>();
        let mut rows = match self.dedup_rows(&embedder) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(error = %error, "{}", crate::i18n::text("memory candidate widening skipped: reading facts failed", "记忆候选扩展跳过:读取事实失败"));
                return 0;
            }
        };
        rows.retain(|row| {
            !known.contains(&row.id)
                && organizer_candidate_is_visible(
                    &row.as_existing(),
                    &allowed_principals,
                    privileged_source,
                )
        });
        let fresh = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.vector.is_none())
            .map(|(index, _)| index)
            .take(MAX_FRESH_EMBEDDINGS)
            .collect::<Vec<_>>();
        if !fresh.is_empty() {
            let texts = fresh
                .iter()
                .map(|index| rows[*index].content.clone())
                .collect::<Vec<_>>();
            match embedder.embed(&texts).await {
                Ok(vectors) => {
                    self.store_vectors(&embedder, &fresh, &rows, &vectors);
                    for (index, vector) in fresh.into_iter().zip(vectors) {
                        rows[index].vector = Some(vector);
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "{}", crate::i18n::text("memory candidate widening kept stored vectors only", "记忆候选扩展只用了存量向量"));
                }
            }
        }
        let floor = embedder.min_score();
        let mut scored = rows
            .into_iter()
            .filter_map(|row| {
                let score = cosine(&query_vector, row.vector.as_ref()?);
                (score >= floor).then_some((score, row))
            })
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let added = scored
            .into_iter()
            .take(WIDEN_LIMIT)
            .map(|(_, row)| row.as_existing())
            .collect::<Vec<_>>();
        let count = added.len();
        batch.existing.extend(added);
        count
    }

    /// 把近义的新建动作折叠成对已有事实的改写，返回改写条数。
    pub(crate) async fn fold_near_duplicate_creates(
        &self,
        batch: &OrganizationBatch,
        output: &mut OrganizedOutput,
    ) -> usize {
        let candidates = batch
            .existing
            .iter()
            .filter(|memory| memory.kind == "knowledge")
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return 0;
        }
        let creates = output
            .knowledge
            .iter()
            .enumerate()
            .filter(|(_, action)| action.operation == "create")
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if creates.is_empty() {
            return 0;
        }

        let mut targets = vec![None; output.knowledge.len()];
        // 归一化文本比较是免费的，先跑一遍，剩下的才值得动用嵌入。
        let mut pending = Vec::new();
        for index in &creates {
            let content = normalized_content(&output.knowledge[*index].content);
            if content.is_empty() {
                continue;
            }
            match candidates
                .iter()
                .find(|memory| normalized_content(&memory.content) == content)
            {
                Some(memory) => targets[*index] = Some(memory.id),
                None => pending.push(*index),
            }
        }
        if !pending.is_empty() {
            if let Some(embedder) = Embedder::from_config(&self.app_config) {
                match self
                    .embedding_matches(&embedder, output, &pending, &candidates)
                    .await
                {
                    Ok(matches) => {
                        for (index, target_id) in matches {
                            targets[index] = Some(target_id);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "{}", crate::i18n::text("memory dedup skipped: embedding failed", "记忆去重跳过:向量计算失败"));
                    }
                }
            }
        }

        let diary_ids = batch
            .diaries
            .iter()
            .map(|diary| diary.id)
            .collect::<BTreeSet<_>>();
        let candidate_fact_ids = candidates.iter().map(|memory| memory.id).collect();
        let candidate_facts = candidates
            .iter()
            .map(|memory| (memory.id, *memory))
            .collect::<BTreeMap<_, _>>();
        let mut folded = 0;
        for index in creates {
            let Some(target_id) = targets[index] else {
                continue;
            };
            let rewritten = knowledge_update(&output.knowledge[index], target_id);
            if !rewrite_is_legal(
                batch,
                &rewritten,
                &diary_ids,
                &candidate_fact_ids,
                &candidate_facts,
            ) {
                continue;
            }
            output.knowledge[index] = rewritten;
            folded += 1;
        }
        folded
    }

    /// 每条待判 create 的最佳近义目标：把两边的文本合成一次嵌入调用，逐条取
    /// 相似度最高的候选。
    async fn embedding_matches(
        &self,
        embedder: &Embedder,
        output: &OrganizedOutput,
        pending: &[usize],
        candidates: &[&ExistingMemoryRecord],
    ) -> Result<Vec<(usize, i64)>> {
        if pending.len() + candidates.len() > MAX_FRESH_EMBEDDINGS {
            return Ok(Vec::new());
        }
        let mut texts = pending
            .iter()
            .map(|index| output.knowledge[*index].content.clone())
            .collect::<Vec<_>>();
        let split = texts.len();
        texts.extend(candidates.iter().map(|memory| memory.content.clone()));
        let vectors = embedder.embed(&texts).await?;
        let mut matches = Vec::new();
        for (offset, index) in pending.iter().enumerate() {
            let mut best: Option<(f32, i64)> = None;
            for (slot, memory) in candidates.iter().enumerate() {
                let score = cosine(&vectors[offset], &vectors[split + slot]);
                if score < NEAR_DUPLICATE_COSINE {
                    continue;
                }
                if best.is_none_or(|(top, _)| score > top) {
                    best = Some((score, memory.id));
                }
            }
            if let Some((_, target_id)) = best {
                matches.push((*index, target_id));
            }
        }
        Ok(matches)
    }

    /// 现算出来的向量顺手存回库里:下一批整理直接复用,不必整库重算。
    fn store_vectors(
        &self,
        embedder: &Embedder,
        indexes: &[usize],
        rows: &[DedupRow],
        vectors: &[Vec<f32>],
    ) {
        let Ok(conn) = self.data_conn() else {
            return;
        };
        let timestamp = now();
        for (index, vector) in indexes.iter().zip(vectors) {
            let row = &rows[*index];
            if let Err(error) = conn.execute(
                "INSERT INTO memory_embeddings (kind, id, model, content_sha256, embedding, created_at)
                 VALUES ('fact', ?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (kind, id) DO UPDATE SET
                    model = excluded.model,
                    content_sha256 = excluded.content_sha256,
                    embedding = excluded.embedding,
                    created_at = excluded.created_at",
                params![
                    row.id,
                    embedder.model_id(),
                    content_sha(&row.content),
                    vector_to_blob(vector),
                    timestamp
                ],
            ) {
                tracing::warn!(error = %error, "{}", crate::i18n::text("storing memory dedup vectors failed", "记忆去重向量落库失败"));
                return;
            }
        }
    }

    /// 库里的事实连同存量向量。连接不跨 await 持有（rusqlite 的 Connection
    /// 不是 Sync），所以取完就丢。
    fn dedup_rows(&self, embedder: &Embedder) -> Result<Vec<DedupRow>> {
        let conn = self.data_conn()?;
        let mut stmt = conn.prepare(
            "SELECT t.id, t.content, t.truth_status, t.visibility, t.owner_principal,
                    t.owner_display_name, e.content_sha256, e.embedding
               FROM facts t
               LEFT JOIN memory_embeddings e
                      ON e.kind='fact' AND e.id=t.id AND e.model=?1
              WHERE t.status!='forgotten' AND t.truth_status!='rejected'
              ORDER BY t.updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(
            params![embedder.model_id(), DEDUP_CORPUS_LIMIT as i64],
            |row| {
                let content = row.get::<_, String>(1)?;
                let stored_sha = row.get::<_, Option<String>>(6)?;
                let blob = row.get::<_, Option<Vec<u8>>>(7)?;
                let vector = match (stored_sha, blob) {
                    (Some(stored), Some(blob)) if stored == content_sha(&content) => {
                        vector_from_blob(&blob)
                    }
                    _ => None,
                };
                Ok(DedupRow {
                    id: row.get(0)?,
                    content,
                    truth_status: row.get(2)?,
                    visibility: row.get(3)?,
                    owner_principal: row.get(4)?,
                    owner_display_name: row.get(5)?,
                    vector,
                })
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn content_sha(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// 同一句换个写法的词面下限：小写、去空白与标点、全角转半角。
fn normalized_content(text: &str) -> String {
    text.chars()
        .filter_map(|ch| {
            let ch = match ch {
                '\u{3000}' => ' ',
                '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
                _ => ch,
            };
            if ch.is_whitespace() {
                return None;
            }
            if !ch.is_alphanumeric() {
                return None;
            }
            Some(ch.to_lowercase().next().unwrap_or(ch))
        })
        .collect()
}

fn knowledge_update(action: &KnowledgeAction, target_id: i64) -> KnowledgeAction {
    let mut rewritten = action.clone();
    rewritten.operation = "update".to_string();
    rewritten.target_id = Some(target_id);
    rewritten
}

/// 改写后必须过与落库同一套校验。任何一条不过就退回 create——被 validate 丢
/// 掉的那条事实比一条近义重复更糟。
fn rewrite_is_legal(
    batch: &OrganizationBatch,
    action: &KnowledgeAction,
    diary_ids: &BTreeSet<i64>,
    candidate_fact_ids: &BTreeSet<i64>,
    candidate_facts: &BTreeMap<i64, &ExistingMemoryRecord>,
) -> bool {
    validate_knowledge_action(action, diary_ids, candidate_fact_ids).is_ok()
        && validate_knowledge_visibility(batch, action).is_ok()
        && validate_knowledge_update_scope(batch, action, candidate_facts).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_content_ignores_case_width_and_punctuation() {
        assert_eq!(
            normalized_content("  2026-09-09 实测:DSH 启动方式是 bunx。"),
            normalized_content("2026-09-09实测，dsh启动方式是bunx")
        );
        assert_eq!(normalized_content("Ａ Ｂ"), normalized_content("ab"));
        assert_ne!(
            normalized_content("本机没装 npm"),
            normalized_content("本机没装 pnpm")
        );
    }

    #[test]
    fn knowledge_update_only_changes_the_target() {
        let action = KnowledgeAction {
            operation: "create".to_string(),
            target_id: None,
            memory_type: "fact".to_string(),
            content: "内容".to_string(),
            truth_status: "reported".to_string(),
            importance: 3,
            confidence: 0.8,
            visibility: "principal".to_string(),
            subjects: Vec::new(),
            tags: Vec::new(),
            diary_ids: vec![1],
        };
        let rewritten = knowledge_update(&action, 7);
        assert_eq!(rewritten.operation, "update");
        assert_eq!(rewritten.target_id, Some(7));
        assert_eq!(rewritten.content, action.content);
        assert_eq!(rewritten.diary_ids, action.diary_ids);
    }
}
