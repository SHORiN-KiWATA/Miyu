//! 会话的增删改查与指针。
//!
//! 「当前会话」不是一个全局值而是一族指针：全局的、每个人格的、REPL 专用的。
//! 分开是因为在网页上切人格不该把终端里正在用的会话也切掉。
//!
//! 人格改名要连带迁移会话归属（`rename_persona_scope`）——scope 是外键性质的
//! 字符串，漏迁就会留下一批指向不存在人格的会话。

use crate::state::conversation_db::*;

impl ConversationDb {
    /// Resolves the current session pointer from `app_state`, self-healing a
    /// missing pointer or dangling session row back to the default session.
    pub fn resolve_current_session(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let pointer: Option<String> = conn
            .query_row(
                "SELECT value FROM app_state WHERE key = 'current_session'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(session_id) = pointer {
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
                params![session_id],
                |row| row.get(0),
            )?;
            if exists {
                return Ok(session_id);
            }
        }
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (session_id, persona, name, kind, created_at, updated_at)
             VALUES (?1, '', ?2, 'user', ?3, ?3)",
            params![
                crate::state::migrations::DEFAULT_SESSION_ID,
                t("Terminal session", "终端集成会话"),
                now
            ],
        )?;
        conn.execute(
            "INSERT INTO app_state (key, value) VALUES ('current_session', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![crate::state::migrations::DEFAULT_SESSION_ID],
        )?;
        Ok(crate::state::migrations::DEFAULT_SESSION_ID.to_string())
    }

    /// Persists the current-session pointer. The target session must exist.
    pub fn set_current_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )?;
        if !exists {
            bail!("session not found: {session_id}");
        }
        conn.execute(
            "INSERT INTO app_state (key, value) VALUES ('current_session', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![session_id],
        )?;
        Ok(())
    }

    /// Reads a persona-scoped session pointer, returning `None` when it points
    /// at something the caller must not land on (wrong persona, non-user kind,
    /// archived, or already deleted). Callers fall back and heal the pointer.
    pub(crate) fn persona_session_pointer(
        &self,
        prefix: &str,
        persona: &str,
    ) -> Result<Option<String>> {
        let key = format!("{prefix}:{persona}");
        let conn = self.conn.lock().unwrap();
        let session_id = conn
            .query_row(
                "SELECT value FROM app_state WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(session_id) = session_id else {
            return Ok(None);
        };
        let valid = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1 AND persona = ?2 AND kind = ?3)",
                params![session_id, persona, crate::state::USER_SESSION_KIND],
                |row| row.get::<_, bool>(0),
            )?;
        Ok(valid.then_some(session_id))
    }

    pub(crate) fn set_persona_session_pointer(
        &self,
        prefix: &str,
        persona: &str,
        session_id: &str,
    ) -> Result<()> {
        let key = format!("{prefix}:{persona}");
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, session_id],
        )?;
        Ok(())
    }

    pub fn persona_current_session(&self, persona: &str) -> Result<Option<String>> {
        self.persona_session_pointer(CURRENT_SESSION_POINTER, persona)
    }

    pub fn set_persona_current_session(&self, persona: &str, session_id: &str) -> Result<()> {
        self.set_persona_session_pointer(CURRENT_SESSION_POINTER, persona, session_id)
    }

    /// The REPL's own lane. Kept apart from the current-session pointer so a
    /// REPL reopens where it left off while shell-hook keeps using the
    /// terminal session it was on.
    pub fn repl_session(&self, persona: &str) -> Result<Option<String>> {
        self.persona_session_pointer(REPL_SESSION_POINTER, persona)
    }

    pub fn set_repl_session(&self, persona: &str, session_id: &str) -> Result<()> {
        self.set_persona_session_pointer(REPL_SESSION_POINTER, persona, session_id)
    }

    /// Claims persona-less sessions (schema-v2 migrated rows) for the given
    /// persona scope. Called once at daemon startup with the active persona.
    pub fn adopt_sessions_for_persona(&self, persona: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET persona = ?1 WHERE persona = ''",
            params![persona],
        )?;
        Ok(())
    }

    pub fn rename_persona_scope(&self, old_scope: &str, new_scope: &str) -> Result<()> {
        if old_scope == new_scope {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let target_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE persona = ?1)",
            params![new_scope],
            |row| row.get(0),
        )?;
        if target_exists {
            bail!("persona scope already has sessions: {new_scope}");
        }
        let old_key = format!("current_session_persona:{old_scope}");
        let new_key = format!("current_session_persona:{new_scope}");
        let target_pointer_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM app_state WHERE key = ?1)",
            params![new_key],
            |row| row.get(0),
        )?;
        if target_pointer_exists {
            bail!("persona scope already has a current-session pointer: {new_scope}");
        }
        let old_affection_key = format!("affection_profile:{old_scope}");
        let new_affection_key = format!("affection_profile:{new_scope}");
        let target_affection_exists: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM platform_plugin_kv
                 WHERE plugin_id = 'real_context' AND key = ?1
             )",
            params![new_affection_key],
            |row| row.get(0),
        )?;
        if target_affection_exists {
            bail!("persona scope already has affection state: {new_scope}");
        }

        tx.execute(
            "UPDATE platform_session_bindings SET persona = ?2 WHERE persona = ?1",
            params![old_scope, new_scope],
        )?;
        tx.execute(
            "UPDATE sessions SET persona = ?2 WHERE persona = ?1",
            params![old_scope, new_scope],
        )?;
        tx.execute(
            "UPDATE app_state SET key = ?2 WHERE key = ?1",
            params![old_key, new_key],
        )?;
        tx.execute(
            "UPDATE platform_plugin_kv SET key = ?2
              WHERE plugin_id = 'real_context' AND key = ?1",
            params![old_affection_key, new_affection_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_persona_scope(&self, scope: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM sessions WHERE persona = ?1", params![scope])?;
        tx.execute(
            "DELETE FROM app_state WHERE key = ?1",
            params![format!("current_session_persona:{scope}")],
        )?;
        tx.execute(
            "DELETE FROM platform_plugin_kv
              WHERE plugin_id = 'real_context' AND key = ?1",
            params![format!("affection_profile:{scope}")],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn session_record(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE session_id = ?1"),
                params![session_id],
                session_record_from_row,
            )
            .optional()?)
    }

    /// User-facing sessions of a persona, most recently updated first.
    /// Subagent sessions (`kind != 'user'`) are excluded.
    pub fn list_sessions(&self, persona: &str) -> Result<Vec<SessionOverview>> {
        self.list_sessions_filtered(persona, false, None)
    }

    /// 某个账号名下的本地会话(阶段 5):`owner` 为空串时是管理员视角——
    /// 看遗留/自己的会话,不看别的账号的。
    pub fn list_local_sessions_for_owner(
        &self,
        persona: &str,
        owner: &str,
    ) -> Result<Vec<SessionOverview>> {
        self.list_sessions_filtered(persona, true, Some(owner))
    }

    /// Local user sessions suitable for CLI/WebUI navigation. Sessions
    /// owned by a messaging-platform binding keep their history but are not
    /// exposed as local conversations.
    pub fn list_local_sessions(&self, persona: &str) -> Result<Vec<SessionOverview>> {
        self.list_sessions_filtered(persona, true, None)
    }

    /// 某个成员名下的全部本地会话,不分人格(成员的会话可能挂在自己的私有
    /// 人格上,阶段 8)。
    pub fn list_owner_sessions(&self, owner: &str) -> Result<Vec<SessionOverview>> {
        self.list_sessions_query(None, true, Some(owner))
    }

    pub(crate) fn list_sessions_filtered(
        &self,
        persona: &str,
        local_only: bool,
        owner: Option<&str>,
    ) -> Result<Vec<SessionOverview>> {
        self.list_sessions_query(Some(persona), local_only, owner)
    }

    fn list_sessions_query(
        &self,
        persona: Option<&str>,
        local_only: bool,
        owner: Option<&str>,
    ) -> Result<Vec<SessionOverview>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS},
                    (SELECT count(*) FROM turns
                      WHERE turns.session_id = sessions.session_id
                        AND hidden = 0 AND is_summary = 0) AS turn_count,
                    (SELECT display_content FROM turns
                      WHERE turns.session_id = sessions.session_id
                        AND hidden = 0 AND is_summary = 0
                      ORDER BY seq DESC LIMIT 1) AS last_user_content
             FROM sessions
             WHERE (?1 IS NULL OR persona = ?1) AND kind = 'user'
               AND (?2 = 0 OR NOT EXISTS (
                    SELECT 1 FROM platform_session_bindings
                    WHERE platform_session_bindings.session_id = sessions.session_id
               ))
               AND (?3 IS NULL OR owner = ?3)
             ORDER BY sort_key ASC, updated_at DESC"
        ))?;
        let rows = stmt.query_map(params![persona, local_only, owner], |row| {
            Ok(SessionOverview {
                record: session_record_from_row(row)?,
                turn_count: row.get("turn_count")?,
                last_user_content: row.get("last_user_content")?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 最老可见轮的用户时间戳(排除指定回合;Utc RFC3339)。联想自回声
    /// 过滤用它当"仍在眼前"的下界:被 compact 藏起的轮不算。
    pub fn oldest_visible_turn_timestamp(
        &self,
        session_id: &str,
        excluding_turn_id: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT MIN(user_timestamp) FROM turns
              WHERE session_id = ?1 AND hidden = 0 AND is_summary = 0 AND turn_id != ?2",
            params![session_id, excluding_turn_id],
            |row| row.get::<_, Option<String>>(0),
        )?)
    }

    pub fn is_platform_session(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM platform_session_bindings WHERE session_id = ?1
            )",
            params![session_id],
            |row| row.get(0),
        )?)
    }

    pub fn persona_reset_session_ids(&self, persona: &str, platform: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE targets(session_id) AS (
                 SELECT sessions.session_id
                   FROM sessions
                  WHERE sessions.persona = ?1
                    AND sessions.kind = 'user'
                    AND (
                        (NOT EXISTS (
                            SELECT 1 FROM platform_session_bindings
                             WHERE platform_session_bindings.session_id = sessions.session_id
                        ))
                        OR EXISTS (
                            SELECT 1 FROM platform_session_bindings
                             WHERE platform_session_bindings.session_id = sessions.session_id
                               AND platform_session_bindings.platform = ?2
                        )
                    )
                 UNION
                 SELECT child.session_id
                   FROM sessions child
                   JOIN targets parent ON child.parent_session_id = parent.session_id
                  WHERE child.persona = ?1
             )
             SELECT session_id FROM targets ORDER BY session_id",
        )?;
        let rows = stmt.query_map(params![persona, platform], |row| row.get(0))?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn create_session(
        &self,
        persona: &str,
        name: &str,
        kind: &str,
        parent_session_id: Option<&str>,
        owner: &str,
    ) -> Result<SessionRecord> {
        let conn = self.conn.lock().unwrap();
        let now = Utc::now().to_rfc3339();
        let session_id = format!(
            "sess_{}_{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0),
            rand::random::<u32>()
        );
        // 新会话插到本人格列表最前(sort_key 越小越靠前;手动排序语义下
        // "最新建的在顶上"是唯一自动行为)。
        conn.execute(
            "INSERT INTO sessions (session_id, persona, name, kind, parent_session_id, created_at, updated_at, sort_key, owner)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6,
                     (SELECT COALESCE(MIN(sort_key), 1024) - 1024 FROM sessions
                       WHERE persona = ?2 AND kind = 'user'), ?7)",
            params![session_id, persona, name, kind, parent_session_id, now, owner],
        )?;
        drop(conn);
        Ok(self
            .session_record(&session_id)?
            .expect("session row just inserted"))
    }

    /// 会话手动排序:按给定顺序重写 sort_key(间隔 1024)。只动 user 会话,
    /// 未列出的行保持原 key(组内拖拽只发本组也不破坏另一组)。
    pub fn reorder_sessions(&self, ordered_ids: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for (index, session_id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE sessions SET sort_key = ?1 WHERE session_id = ?2 AND kind = 'user'",
                params![(index as i64 + 1) * 1024, session_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn create_or_get_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        name: &str,
    ) -> Result<(SessionRecord, bool)> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(session_id) = tx
            .query_row(
                "SELECT session_id FROM platform_session_bindings
                 WHERE platform = ?1 AND account_id = ?2
                   AND conversation_kind = ?3 AND conversation_id = ?4
                   AND participant_id = ?5 AND persona = ?6",
                params![
                    key.platform,
                    key.account_id,
                    key.conversation_kind,
                    key.conversation_id,
                    key.normalized_participant_id(),
                    key.persona,
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            let record = tx.query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE session_id = ?1"),
                params![session_id],
                session_record_from_row,
            )?;
            tx.commit()?;
            return Ok((record, false));
        }

        let now = Utc::now().to_rfc3339();
        let session_id = format!(
            "sess_{}_{:08x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis())
                .unwrap_or(0),
            rand::random::<u32>()
        );
        tx.execute(
            "INSERT INTO sessions (session_id, persona, name, kind, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'user', ?4, ?4)",
            params![session_id, key.persona, name, now],
        )?;
        tx.execute(
            "INSERT INTO platform_session_bindings (
                platform, account_id, conversation_kind, conversation_id,
                participant_id, persona, session_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                key.platform,
                key.account_id,
                key.conversation_kind,
                key.conversation_id,
                key.normalized_participant_id(),
                key.persona,
                session_id,
                now,
            ],
        )?;
        let record = SessionRecord {
            session_id,
            persona: key.persona.clone(),
            name: name.to_string(),
            kind: "user".to_string(),
            parent_session_id: None,
            workspace: None,
            archived: false,
            created_at: now.clone(),
            updated_at: now,
            sort_key: 0,
            owner: String::new(),
        };
        tx.commit()?;
        Ok((record, true))
    }

    pub fn rename_session(&self, session_id: &str, name: &str) -> Result<()> {
        self.update_session_field(session_id, "name", Some(name))
    }

    /// 改会话的人格 scope。只给还没聊过的空会话用(切人格时跟着换)。
    pub fn set_session_persona(&self, session_id: &str, persona: &str) -> Result<()> {
        self.update_session_field(session_id, "persona", Some(persona))
    }

    pub fn set_session_workspace(&self, session_id: &str, workspace: Option<&str>) -> Result<()> {
        self.update_session_field(session_id, "workspace", workspace)
    }

    /// JSON-encoded per-session model pool override
    /// (`[{"provider_id": ..., "model": ...}, ...]`); None follows the global
    /// active pool.
    pub fn session_model_override(&self, session_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let value = conn
            .query_row(
                "SELECT model_override FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    pub fn set_session_model_override(&self, session_id: &str, value: Option<&str>) -> Result<()> {
        self.update_session_field(session_id, "model_override", value)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // queued_prompts gained session_id through an ALTER TABLE migration,
        // so existing databases cannot rely on an ON DELETE foreign key.
        tx.execute(
            "DELETE FROM queued_prompts WHERE session_id = ?1",
            params![session_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        )?;
        if deleted == 0 {
            bail!("session not found: {session_id}");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn touch_session(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE session_id = ?1",
            params![session_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn find_session_by_name(&self, persona: &str, name: &str) -> Result<Option<SessionRecord>> {
        self.find_session_by_name_filtered(persona, name, false)
    }

    pub fn find_local_session_by_name(
        &self,
        persona: &str,
        name: &str,
    ) -> Result<Option<SessionRecord>> {
        self.find_session_by_name_filtered(persona, name, true)
    }

    pub(crate) fn find_session_by_name_filtered(
        &self,
        persona: &str,
        name: &str,
        local_only: bool,
    ) -> Result<Option<SessionRecord>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM sessions
                      WHERE persona = ?1 AND kind = 'user' AND name = ?2 COLLATE NOCASE
                        AND (?3 = 0 OR NOT EXISTS (
                            SELECT 1 FROM platform_session_bindings
                             WHERE platform_session_bindings.session_id = sessions.session_id
                        ))
                      ORDER BY archived ASC, updated_at DESC LIMIT 1"
                ),
                params![persona, name, local_only],
                session_record_from_row,
            )
            .optional()?)
    }

    /// Deletes subagent audit sessions older than the retention window;
    /// their turns/images/queues cascade away.
    pub fn delete_subagent_sessions_older_than(&self, days: i64) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // 与 delete_ask_sessions_older_than 同理:queued_prompts.session_id
        // 经 ALTER 而来没有级联外键,必须先手动清,否则留孤儿行。
        tx.execute(
            "DELETE FROM queued_prompts WHERE session_id IN (
                 SELECT session_id FROM sessions
                 WHERE kind = 'subagent'
                   AND datetime(updated_at) < datetime('now', '-' || ?1 || ' days'))",
            params![days],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sessions
             WHERE kind = 'subagent'
               AND datetime(updated_at) < datetime('now', '-' || ?1 || ' days')",
            params![days],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Deletes abandoned one-shot sessions older than the retention window. A
    /// `miyu ask` turn deletes its own session; anything still here was
    /// orphaned by a client that died mid-turn (Ctrl+C, SIGKILL).
    pub fn delete_ask_sessions_older_than(&self, hours: i64) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // queued_prompts.session_id arrived via ALTER and has no cascading FK,
        // so its rows have to go first (same reason as `delete_session`).
        tx.execute(
            "DELETE FROM queued_prompts WHERE session_id IN (
                 SELECT session_id FROM sessions
                 WHERE kind = ?1
                   AND datetime(updated_at) < datetime('now', '-' || ?2 || ' hours'))",
            params![crate::state::ASK_SESSION_KIND, hours],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sessions
             WHERE kind = ?1
               AND datetime(updated_at) < datetime('now', '-' || ?2 || ' hours')",
            params![crate::state::ASK_SESSION_KIND, hours],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub(crate) fn update_session_field(
        &self,
        session_id: &str,
        field: &'static str,
        value: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            &format!("UPDATE sessions SET {field} = ?2, updated_at = ?3 WHERE session_id = ?1"),
            params![session_id, value, Utc::now().to_rfc3339()],
        )?;
        if updated == 0 {
            bail!("session not found: {session_id}");
        }
        Ok(())
    }

    /// Lifetime token total of one session, summed over every turn row —
    /// including hidden (compacted) turns and summary rows, so the counter
    /// keeps growing across compactions and only /reset (which deletes the
    /// rows) brings it back to zero.
    pub fn session_token_total(&self, session_id: &str) -> Result<u64> {
        Ok(self.session_token_totals(session_id)?.total)
    }

    /// Session-lifetime sums behind the Σ meter. Returned together because the
    /// cumulative cache rate is `cache_read / prompt` and reading the two
    /// halves through separate locks could straddle a turn commit.
    pub fn session_token_totals(&self, session_id: &str) -> Result<TurnTokens> {
        let conn = self.conn.lock().unwrap();
        let (total, prompt, cache_read): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(token_total), 0), COALESCE(SUM(token_prompt), 0),
                    COALESCE(SUM(token_cache_read), 0)
             FROM turns WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        // Subagents bill to the session that launched them: their audit
        // sessions hang off this one, and a Σ that ignored them would hide the
        // single biggest thing a turn can spend. Estimated runs land in
        // `total_tokens` only — `prompt_tokens` stays 0 when the provider
        // reported nothing — so a guessed number can inflate Σ but never
        // reaches the cache rate's denominator.
        let (sub_total, sub_prompt, sub_cache): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(CASE WHEN cache_read_tokens IS NULL THEN 0
                                      ELSE prompt_tokens END), 0),
                    COALESCE(SUM(cache_read_tokens), 0)
             FROM sessions WHERE parent_session_id = ?1 AND kind = 'subagent'",
            rusqlite::params![session_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(TurnTokens {
            total: total.saturating_add(sub_total).max(0) as u64,
            prompt: prompt.saturating_add(sub_prompt).max(0) as u64,
            cache_read: cache_read.saturating_add(sub_cache).max(0) as u64,
        })
    }
}
