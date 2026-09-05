//! 召回：查记忆、查往事、生成联想。
//!
//! 召回本身是**有副作用**的：`reinforce` 会加强被召回的记忆，`decay_memories`
//! 按半衰期削弱没被召回的。这是遗忘曲线的实现，不是缓存。
//!
//! `retain_unseen_association` 是注入前的最后一道：已经在可见历史里的内容不再
//! 重复注入。

use crate::memory::*;

impl MemoryStore {
    pub fn recall_memories(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        self.init()?;
        self.recall_memories_existing(query, limit, include_forgotten)
    }

    pub fn recall_memories_readonly(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        if !self.data_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "facts": [], "episodes": [] }));
        }
        self.recall_memories_existing(query, limit, include_forgotten)
    }

    pub(crate) fn recall_memories_existing(
        &self,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Value> {
        let conn = self.data_conn()?;
        let facts = self.search_facts(&conn, query, limit, include_forgotten)?;
        let episodes = self.search_episodes(&conn, query, limit, include_forgotten)?;
        Ok(json!({
            "ok": true,
            "query": query,
            "facts": facts.iter().map(memory_hit_json).collect::<Vec<_>>(),
            "episodes": episodes.iter().map(memory_hit_json).collect::<Vec<_>>(),
        }))
    }

    #[allow(dead_code)]
    pub fn recall_past_events(&self, query: &str, limit: usize) -> Result<Value> {
        self.init()?;
        self.recall_past_events_existing(query, limit)
    }

    /// 按 id 取一条记忆的全文。联想块里的日记条目会被单条上限截断并附上
    /// `recall_memories id=<id>`,这是那条提示的落点(08-17)。
    /// 访问控制与检索同口径:principal 会话只能看到 public 或自己的记录。
    pub fn recall_by_id_readonly(&self, id: i64) -> Result<Value> {
        if !self.data_db.is_file() {
            return Ok(json!({ "ok": false, "id": id, "error": "memory database is empty" }));
        }
        let conn = self.data_conn()?;
        for (table, kind) in [("episodes", MemoryKind::Diary), ("facts", MemoryKind::Fact)] {
            let access_filter = if self.access.principal_key().is_some() {
                " AND (visibility='public' OR (visibility='principal' AND owner_principal=?2))"
            } else {
                ""
            };
            let sql = format!(
                "SELECT id, content, source, status, created_at,
                        visibility, owner_principal, owner_display_name, subjects, {}
                 FROM {table} WHERE id=?1{access_filter}",
                if kind == MemoryKind::Diary {
                    "retention"
                } else {
                    "NULL"
                },
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut rows = match self.access.principal_key() {
                Some(principal) => stmt.query(params![id, principal])?,
                None => stmt.query(params![id])?,
            };
            if let Some(row) = rows.next()? {
                return Ok(json!({
                    "ok": true,
                    "id": row.get::<_, i64>(0)?,
                    "kind": match kind { MemoryKind::Fact => "knowledge", MemoryKind::Diary => "diary" },
                    "content": row.get::<_, String>(1)?,
                    "source": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "timestamp": row.get::<_, String>(4)?,
                    "visibility": row.get::<_, String>(5)?,
                    "owner_principal": row.get::<_, String>(6)?,
                    "owner_display_name": truncate_chars(&compact_line(&row.get::<_, String>(7)?), 128),
                    "subjects": serde_json::from_str::<Value>(&row.get::<_, String>(8)?).unwrap_or_else(|_| json!([])),
                    "retention": row.get::<_, Option<String>>(9)?,
                }));
            }
        }
        Ok(json!({ "ok": false, "id": id, "error": "no memory with this id is visible here" }))
    }

    pub fn recall_past_events_readonly(&self, query: &str, limit: usize) -> Result<Value> {
        if !self.data_db.is_file() {
            return Ok(json!({ "ok": true, "query": query, "episodes": [] }));
        }
        self.recall_past_events_existing(query, limit)
    }

    pub(crate) fn recall_past_events_existing(&self, query: &str, limit: usize) -> Result<Value> {
        let conn = self.data_conn()?;
        let episodes = self.search_episodes(&conn, query, limit, true)?;
        Ok(json!({
            "ok": true,
            "query": query,
            "episodes": episodes.iter().map(memory_hit_json).collect::<Vec<_>>(),
        }))
    }

    pub fn association(
        &self,
        query: &str,
        exclude: Option<&AssociationExclusion>,
    ) -> Result<Option<AssociationContext>> {
        if !self.config.enabled || !self.config.association_enabled {
            return Ok(None);
        }
        // 一条连接贯穿本回合的两次检索与全部 reinforce,替代此前最多 10 次
        // Connection::open + PRAGMA 重设。
        let conn = self.data_conn()?;
        let (facts, episodes) = self.association_candidates(&conn, query, exclude)?;
        self.finish_association(&conn, facts, episodes)
    }

    /// Keyword candidates for one turn, with the self-echo exclusion applied.
    /// Split from [`Self::association`] so the semantic pass can fuse its own
    /// candidates in before reinforcement happens.
    pub(crate) fn association_candidates(
        &self,
        conn: &Connection,
        query: &str,
        exclude: Option<&AssociationExclusion>,
    ) -> Result<(Vec<MemoryHit>, Vec<MemoryHit>)> {
        let facts = self.search_facts(conn, query, self.config.association_facts, false)?;
        let mut episodes =
            self.search_episodes(conn, query, self.config.association_episodes, false)?;
        self.apply_self_echo_exclusion(&mut episodes, exclude);
        Ok((facts, episodes))
    }

    /// 自回声过滤(缓存调研 08-16):当前会话可见范围内刚写下的日记/
    /// 事实,原对话就在眼前,复述一遍纯属冗余;被 compact 折走后
    /// (时间早于最老可见轮)重新够格召回。显式 recall 工具不受此限。
    pub(crate) fn apply_self_echo_exclusion(
        &self,
        episodes: &mut Vec<MemoryHit>,
        exclude: Option<&AssociationExclusion>,
    ) {
        if let Some(exclude) = exclude {
            // facts 无 origin 列(origin_session_id 恒空串),天然不命中;
            // 实际的自回声源=上一轮自动日记(episodes)。
            episodes.retain(|hit| {
                !(hit.origin_session_id == exclude.session_id && hit.timestamp >= exclude.since)
            });
        }
    }

    /// Retention filtering, reinforcement and packaging of the final
    /// candidate lists. Shared by the keyword-only and the fused paths.
    pub(crate) fn finish_association(
        &self,
        conn: &Connection,
        facts: Vec<MemoryHit>,
        mut episodes: Vec<MemoryHit>,
    ) -> Result<Option<AssociationContext>> {
        let matched_short_ids = episodes
            .iter()
            .filter(|hit| hit.retention.as_deref() == Some(SHORT_TERM))
            .map(|hit| hit.id)
            .collect::<BTreeSet<_>>();
        episodes.retain(|hit| {
            hit.retention.as_deref() == Some(SHORT_TERM)
                || hit
                    .source_episode_ids
                    .iter()
                    .all(|id| !matched_short_ids.contains(id))
        });
        let mut organization_due = false;
        for hit in facts.iter().chain(episodes.iter()) {
            organization_due |= self.reinforce(conn, hit)?;
        }
        if facts.is_empty() && episodes.is_empty() {
            return Ok(None);
        }
        Ok(Some(AssociationContext {
            facts,
            episodes,
            organization_due,
        }))
    }

    pub fn format_association(&self, association: &AssociationContext) -> String {
        let max_chars = self.config.association_max_chars;
        let entry_max_chars = self.config.association_entry_chars;
        if max_chars < 64 {
            return String::new();
        }
        const CLOSING: &str = "</associative-memory>";
        let mut output = String::new();
        output.push_str("<associative-memory>\n");
        // 前言常量已上提到 system 提示词(08-17),这里只留会变的部分:
        // Privileged 一个字都不用写,Principal 只留当前 principal。
        if let MemoryAccess::Principal(principal) = &self.access {
            output.push_str("principal=");
            output.push_str(principal);
            output.push('\n');
        }
        append_association_section(
            &mut output,
            "曾经记住的相关知识点",
            association.facts.iter(),
            &self.access,
            max_chars,
            entry_max_chars,
            CLOSING,
        );
        let short_diaries = association
            .episodes
            .iter()
            .filter(|hit| hit.retention.as_deref() == Some(SHORT_TERM))
            .collect::<Vec<_>>();
        append_association_section(
            &mut output,
            "近期发生的事情",
            short_diaries,
            &self.access,
            max_chars,
            entry_max_chars,
            CLOSING,
        );
        let long_diaries = association
            .episodes
            .iter()
            .filter(|hit| hit.retention.as_deref() != Some(SHORT_TERM))
            .collect::<Vec<_>>();
        append_association_section(
            &mut output,
            "长期保留的经历",
            long_diaries,
            &self.access,
            max_chars,
            entry_max_chars,
            CLOSING,
        );
        let closing_chars = CLOSING.chars().count();
        if output.chars().count() + closing_chars > max_chars {
            output = truncate_chars(&output, max_chars.saturating_sub(closing_chars));
        }
        output.push_str(CLOSING);
        truncate_chars(&output, max_chars)
    }

    pub fn association_dedup_enabled(&self) -> bool {
        self.config.association_dedup
    }

    /// 过滤掉「渲染行已在本次请求上下文中可见」的命中（早前回合的化石逐字回放
    /// 时携带了同一行）。只缩小当前回合新生成的块；历史化石一字节不改写，
    /// append-only 回放与供应商前缀缓存均不受影响。命中被过滤不影响
    /// `association()` 内已完成的 reinforce 记账。
    pub fn retain_unseen_association(
        &self,
        association: &mut AssociationContext,
        seen: &HashSet<&str>,
    ) {
        if seen.is_empty() {
            return;
        }
        let access = &self.access;
        // 去重键必须与真正注入的那一行逐字一致,所以同样吃单条上限。
        let entry_max_chars = self.config.association_entry_chars;
        association.facts.retain(|hit| {
            !seen.contains(association_entry_line(hit, access, entry_max_chars).trim_end())
        });
        association.episodes.retain(|hit| {
            !seen.contains(association_entry_line(hit, access, entry_max_chars).trim_end())
        });
    }

    pub(crate) fn search_facts(
        &self,
        conn: &Connection,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        self.search_table(
            conn,
            "facts",
            MemoryKind::Fact,
            query,
            limit,
            include_forgotten,
        )
    }

    pub(crate) fn search_episodes(
        &self,
        conn: &Connection,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        self.search_table(
            conn,
            "episodes",
            MemoryKind::Diary,
            query,
            limit,
            include_forgotten,
        )
    }

    pub(crate) fn search_table(
        &self,
        conn: &Connection,
        table: &str,
        kind: MemoryKind,
        query: &str,
        limit: usize,
        include_forgotten: bool,
    ) -> Result<Vec<MemoryHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let tokens = query_tokens(query);
        // 归一化与行无关,提到 5000 行循环外做一次。
        let normalized_query = compact_line(query).to_ascii_lowercase();
        let status_filter = if kind == MemoryKind::Fact && include_forgotten {
            "WHERE truth_status!='rejected'"
        } else if kind == MemoryKind::Fact {
            "WHERE status!='forgotten' AND truth_status!='rejected'"
        } else if include_forgotten {
            ""
        } else {
            "WHERE status!='forgotten'"
        };
        let access_filter = if self.access.principal_key().is_some() && status_filter.is_empty() {
            "WHERE visibility='public' OR (visibility='principal' AND owner_principal=?1)"
        } else if self.access.principal_key().is_some() {
            " AND (visibility='public' OR (visibility='principal' AND owner_principal=?1))"
        } else {
            ""
        };
        let sql = format!(
            "SELECT id, content, source, status, created_at, strength,
                     COALESCE(importance, 3), {}, COALESCE(source_episode_ids, '[]'),
                     visibility, owner_principal, owner_display_name, subjects,
                     {}
             FROM {table} {}{} ORDER BY updated_at DESC LIMIT 5000",
            if kind == MemoryKind::Diary {
                "retention"
            } else {
                "NULL"
            },
            // 自回声排除只针对自动日记;facts 表没有 origin 列。
            if kind == MemoryKind::Diary {
                "COALESCE(origin_session_id, '')"
            } else {
                "''"
            },
            status_filter,
            access_filter,
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = match self.access.principal_key() {
            Some(principal) => stmt.query([principal])?,
            None => stmt.query([])?,
        };
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let (mut hit, status, strength, importance) = map_hit_row(row, kind)?;
            if !include_forgotten && status == "forgotten" {
                continue;
            }
            let lexical_score = score_text(&hit.content, &normalized_query, &tokens);
            if lexical_score <= 0.0 {
                continue;
            }
            hit.score = lexical_score
                + strength.clamp(0.0, 1.0) as f32 * 5.0
                + importance.clamp(1, 5) as f32;
            hits.push(hit);
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit.min(50));
        Ok(hits)
    }

    /// Rows for specific ids in the same shape as [`Self::search_table`],
    /// honouring the same status and access filters; `score` is left at 0 for
    /// the caller to assign. Order follows `ids`.
    pub(crate) fn hits_by_ids(
        &self,
        conn: &Connection,
        kind: MemoryKind,
        ids: &[i64],
    ) -> Result<Vec<MemoryHit>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let table = match kind {
            MemoryKind::Fact => "facts",
            MemoryKind::Diary => "episodes",
        };
        let status_filter = if kind == MemoryKind::Fact {
            "status!='forgotten' AND truth_status!='rejected'"
        } else {
            "status!='forgotten'"
        };
        let access_filter = if self.access.principal_key().is_some() {
            " AND (visibility='public' OR (visibility='principal' AND owner_principal=?1))"
        } else {
            ""
        };
        let placeholders = ids
            .iter()
            .enumerate()
            .map(|(index, _)| {
                format!(
                    "?{}",
                    index
                        + if self.access.principal_key().is_some() {
                            2
                        } else {
                            1
                        }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, content, source, status, created_at, strength,
                     COALESCE(importance, 3), {}, COALESCE(source_episode_ids, '[]'),
                     visibility, owner_principal, owner_display_name, subjects,
                     {}
             FROM {table} WHERE {status_filter}{access_filter} AND id IN ({placeholders})",
            if kind == MemoryKind::Diary {
                "retention"
            } else {
                "NULL"
            },
            if kind == MemoryKind::Diary {
                "COALESCE(origin_session_id, '')"
            } else {
                "''"
            },
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(ids.len() + 1);
        if let Some(principal) = self.access.principal_key() {
            params.push(principal.to_string().into());
        }
        params.extend(ids.iter().map(|id| rusqlite::types::Value::from(*id)));
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let mut by_id = std::collections::HashMap::new();
        while let Some(row) = rows.next()? {
            let (hit, _, _, _) = map_hit_row(row, kind)?;
            by_id.insert(hit.id, hit);
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    pub(crate) fn reinforce(&self, conn: &Connection, hit: &MemoryHit) -> Result<bool> {
        self.reinforce_inner(conn, hit)
    }
}

/// The fixed 14-column row shape shared by every hit query; returns the hit
/// (score 0) plus the columns that only the lexical scorer needs.
pub(crate) fn map_hit_row(
    row: &rusqlite::Row<'_>,
    kind: MemoryKind,
) -> rusqlite::Result<(MemoryHit, String, f64, i64)> {
    let id = row.get::<_, i64>(0)?;
    let content = row.get::<_, String>(1)?;
    let source = row.get::<_, String>(2)?;
    let status = row.get::<_, String>(3)?;
    let timestamp = row.get::<_, String>(4)?;
    let strength = row.get::<_, f64>(5)?;
    let importance = row.get::<_, i64>(6)?;
    let retention = row.get::<_, Option<String>>(7)?;
    let source_episode_ids = row.get::<_, String>(8)?;
    let visibility = row.get::<_, String>(9)?;
    let owner_principal = row.get::<_, String>(10)?;
    let owner_display_name = row.get::<_, String>(11)?;
    let subjects = row.get::<_, String>(12)?;
    let origin_session_id = row.get::<_, String>(13)?;
    Ok((
        MemoryHit {
            id,
            origin_session_id,
            kind,
            content,
            score: 0.0,
            timestamp,
            source,
            retention,
            visibility,
            owner_principal,
            owner_display_name,
            subjects,
            source_episode_ids: serde_json::from_str(&source_episode_ids).unwrap_or_default(),
        },
        status,
        strength,
        importance,
    ))
}

impl MemoryStore {
    fn reinforce_inner(&self, conn: &Connection, hit: &MemoryHit) -> Result<bool> {
        let timestamp = now();
        if hit.kind == MemoryKind::Fact {
            conn.execute(
                "UPDATE facts SET recall_count=recall_count+1,
                    strength=MIN(1.0, strength+?1), last_recalled_at=?2,
                    updated_at=?2, status='active' WHERE id=?3",
                params![self.config.forgetting_review_boost, timestamp, hit.id],
            )?;
            return Ok(false);
        }

        let refreshed_expiry = (Utc::now()
            + ChronoDuration::days(self.config.short_diary_retention_days as i64))
        .to_rfc3339();
        conn.execute(
            "UPDATE episodes SET
                recall_count=recall_count+1,
                strength=MIN(1.0, strength+?1),
                last_recalled_at=?2,
                updated_at=?2,
                status='active',
                expires_at=CASE
                    WHEN retention='short_term' AND promoted_at IS NULL THEN ?3
                    ELSE expires_at END,
                promotion_pending=CASE
                    WHEN retention='short_term' AND promoted_at IS NULL
                         AND recall_count+1>=?4 THEN 1
                    ELSE promotion_pending END
             WHERE id=?5",
            params![
                self.config.forgetting_review_boost,
                timestamp,
                refreshed_expiry,
                self.config.diary_promotion_recalls as i64,
                hit.id
            ],
        )?;
        Ok(conn.query_row(
            "SELECT retention='short_term' AND promotion_pending=1
             FROM episodes WHERE id=?1",
            [hit.id],
            |row| row.get::<_, bool>(0),
        )?)
    }

    pub(crate) fn decay_memories(&self) -> Result<()> {
        if !self.config.enabled || !self.config.forgetting_enabled {
            return Ok(());
        }
        let conn = self.data_conn()?;
        self.decay_memories_with_conn(&conn)
    }

    pub(crate) fn decay_memories_with_conn(&self, conn: &Connection) -> Result<()> {
        if !self.config.enabled || !self.config.forgetting_enabled {
            return Ok(());
        }
        decay_table(conn, "facts", &self.config)?;
        decay_table(conn, "episodes", &self.config)?;
        Ok(())
    }
}
