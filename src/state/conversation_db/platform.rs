//! 平台侧的绑定、授权与插件存储。
//!
//! 会话与平台会话（某个群、某个人）的绑定是**多对一**的：换人格会开新会话，但
//! 平台那边还是同一个群。`claim_platform_session` 处理并发抢绑。
//!
//! 授权变更会写审计（`insert_platform_access_audit`）：谁在什么时候把谁加进了
//! 白名单，出问题时这是唯一的线索。
//!
//! 插件的 JSON 存储带版本号（`plugin_get_json_with_revision`），让插件能做乐观
//! 并发而不必各自建表。

use crate::state::conversation_db::*;

impl ConversationDb {
    pub fn platform_session_bindings(
        &self,
        persona: &str,
        platform: &str,
    ) -> Result<Vec<PlatformSessionBinding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT platform, account_id, conversation_kind, conversation_id,
                    participant_id, persona, session_id
               FROM platform_session_bindings
              WHERE persona = ?1 AND platform = ?2
              ORDER BY account_id, conversation_kind, conversation_id, participant_id",
        )?;
        let rows = stmt.query_map(params![persona, platform], |row| {
            let participant_id: String = row.get(4)?;
            Ok(PlatformSessionBinding {
                key: PlatformSessionBindingKey {
                    platform: row.get(0)?,
                    account_id: row.get(1)?,
                    conversation_kind: row.get(2)?,
                    conversation_id: row.get(3)?,
                    participant_id: (!participant_id.is_empty()).then_some(participant_id),
                    persona: row.get(5)?,
                },
                session_id: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn find_platform_session_binding(
        &self,
        key: &PlatformSessionBindingKey,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
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
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Binds an external conversation identity to a session in one immediate
    /// transaction. A key may be reassigned, but a session already owned by a
    /// different key is never stolen.
    pub fn bind_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        session_id: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let session_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            params![session_id],
            |row| row.get(0),
        )?;
        if !session_exists {
            bail!("session not found: {session_id}");
        }

        let owned_by_another_key: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM platform_session_bindings
                 WHERE session_id = ?7
                   AND NOT (
                       platform = ?1 AND account_id = ?2
                       AND conversation_kind = ?3 AND conversation_id = ?4
                       AND participant_id = ?5 AND persona = ?6
                   )
             )",
            params![
                key.platform,
                key.account_id,
                key.conversation_kind,
                key.conversation_id,
                key.normalized_participant_id(),
                key.persona,
                session_id,
            ],
            |row| row.get(0),
        )?;
        if owned_by_another_key {
            bail!("session is already bound to another platform conversation: {session_id}");
        }

        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO platform_session_bindings (
                platform, account_id, conversation_kind, conversation_id,
                participant_id, persona, session_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT (
                platform, account_id, conversation_kind, conversation_id,
                participant_id, persona
             ) DO UPDATE SET
                session_id = excluded.session_id,
                updated_at = excluded.updated_at",
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
        tx.commit()?;
        Ok(())
    }

    /// Claims an unbound external key without replacing an existing binding.
    /// Returns the winning session id so concurrent first messages converge
    /// on one history instead of creating two active sessions.
    pub fn claim_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        candidate_session_id: &str,
    ) -> Result<String> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = tx
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
            tx.commit()?;
            return Ok(existing);
        }
        let session_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = ?1)",
            params![candidate_session_id],
            |row| row.get(0),
        )?;
        if !session_exists {
            bail!("session not found: {candidate_session_id}");
        }
        let already_owned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM platform_session_bindings WHERE session_id = ?1)",
            params![candidate_session_id],
            |row| row.get(0),
        )?;
        if already_owned {
            bail!(
                "session is already bound to another platform conversation: {candidate_session_id}"
            );
        }
        let now = Utc::now().to_rfc3339();
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
                candidate_session_id,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(candidate_session_id.to_string())
    }

    pub fn unbind_platform_session(&self, key: &PlatformSessionBindingKey) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM platform_session_bindings
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
        )?;
        Ok(deleted != 0)
    }

    pub fn platform_access_grants(
        &self,
        platform: Option<&str>,
    ) -> Result<Vec<PlatformAccessGrant>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT
                 platform, account_scope, permission, subject_kind, subject_id,
                 granted_by_platform, granted_by_account_id, granted_by_user_id,
                 granted_conversation_kind, granted_conversation_id,
                 granted_message_id, created_at
             FROM platform_access_grants
             WHERE (?1 IS NULL OR platform = ?1)
             ORDER BY platform, account_scope, permission, subject_kind, subject_id",
        )?;
        let rows = statement.query_map(params![platform], |row| {
            Ok(PlatformAccessGrant {
                key: PlatformAccessGrantKey {
                    platform: row.get("platform")?,
                    account_scope: row.get("account_scope")?,
                    permission: row.get("permission")?,
                    subject_kind: row.get("subject_kind")?,
                    subject_id: row.get("subject_id")?,
                },
                granted_by: PlatformAccessActor {
                    platform: row.get("granted_by_platform")?,
                    account_id: row.get("granted_by_account_id")?,
                    user_id: row.get("granted_by_user_id")?,
                    conversation_kind: row.get("granted_conversation_kind")?,
                    conversation_id: row.get("granted_conversation_id")?,
                    message_id: row.get("granted_message_id")?,
                },
                created_at: row.get("created_at")?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn add_platform_access_grant(
        &self,
        key: &PlatformAccessGrantKey,
        actor: &PlatformAccessActor,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let created_at = Utc::now().to_rfc3339();
        let inserted = tx.execute(
            "INSERT INTO platform_access_grants (
                 platform, account_scope, permission, subject_kind, subject_id,
                 granted_by_platform, granted_by_account_id, granted_by_user_id,
                 granted_conversation_kind, granted_conversation_id,
                 granted_message_id, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (
                 platform, account_scope, permission, subject_kind, subject_id
             ) DO NOTHING",
            params![
                key.platform,
                key.account_scope,
                key.permission,
                key.subject_kind,
                key.subject_id,
                actor.platform,
                actor.account_id,
                actor.user_id,
                actor.conversation_kind,
                actor.conversation_id,
                actor.message_id,
                created_at,
            ],
        )?;
        if inserted != 0 {
            insert_platform_access_audit(&tx, "grant", key, actor, &created_at)?;
        }
        tx.commit()?;
        Ok(inserted != 0)
    }

    pub fn remove_platform_access_grant(
        &self,
        key: &PlatformAccessGrantKey,
        actor: &PlatformAccessActor,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM platform_access_grants
             WHERE platform = ?1 AND account_scope = ?2 AND permission = ?3
               AND subject_kind = ?4 AND subject_id = ?5",
            params![
                key.platform,
                key.account_scope,
                key.permission,
                key.subject_kind,
                key.subject_id,
            ],
        )?;
        if deleted != 0 {
            let created_at = Utc::now().to_rfc3339();
            insert_platform_access_audit(&tx, "revoke", key, actor, &created_at)?;
        }
        tx.commit()?;
        Ok(deleted != 0)
    }

    pub fn plugin_get_json<T: DeserializeOwned>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<T>> {
        let conn = self.conn.lock().unwrap();
        let value_json = conn
            .query_row(
                "SELECT value_json FROM platform_plugin_kv
                 WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
                   AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
                params![
                    scope.plugin_id,
                    scope.platform,
                    scope.account_id,
                    scope.conversation_kind,
                    scope.conversation_id,
                    key,
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        drop(conn);
        value_json
            .map(|value| serde_json::from_str(&value).context("invalid platform plugin JSON state"))
            .transpose()
    }

    pub fn plugin_json_revision(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT updated_at FROM platform_plugin_kv
             WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
               AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
            params![
                scope.plugin_id,
                scope.platform,
                scope.account_id,
                scope.conversation_kind,
                scope.conversation_id,
                key,
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn plugin_get_json_with_revision<T: DeserializeOwned>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<(T, String)>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT value_json, updated_at FROM platform_plugin_kv
                 WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
                   AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
                params![
                    scope.plugin_id,
                    scope.platform,
                    scope.account_id,
                    scope.conversation_kind,
                    scope.conversation_id,
                    key,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        drop(conn);
        row.map(|(value, revision)| {
            serde_json::from_str(&value)
                .context("invalid platform plugin JSON state")
                .map(|value| (value, revision))
        })
        .transpose()
    }

    pub fn plugin_put_json<T: Serialize + ?Sized>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
        value: &T,
    ) -> Result<()> {
        let value_json =
            serde_json::to_string(value).context("failed to serialize platform plugin state")?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO platform_plugin_kv (
                plugin_id, platform, account_id, conversation_kind,
                conversation_id, key, value_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (
                plugin_id, platform, account_id, conversation_kind,
                conversation_id, key
             ) DO UPDATE SET
                value_json = excluded.value_json,
                updated_at = excluded.updated_at",
            params![
                scope.plugin_id,
                scope.platform,
                scope.account_id,
                scope.conversation_kind,
                scope.conversation_id,
                key,
                value_json,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Atomically replaces one plugin value. Returning `None` deletes it.
    /// The callback runs inside an immediate transaction and must not re-enter
    /// this database connection.
    pub fn plugin_update_json<T, F>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
        update: F,
    ) -> Result<Option<T>>
    where
        T: DeserializeOwned + Serialize,
        F: FnOnce(Option<T>) -> Result<Option<T>>,
    {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value_json = tx
            .query_row(
                "SELECT value_json FROM platform_plugin_kv
                 WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
                   AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
                params![
                    scope.plugin_id,
                    scope.platform,
                    scope.account_id,
                    scope.conversation_kind,
                    scope.conversation_id,
                    key,
                ],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let current = value_json
            .map(|value| serde_json::from_str(&value).context("invalid platform plugin JSON state"))
            .transpose()?;
        let current_json = current
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize platform plugin state")?;
        let next = update(current)?;
        let next_json = next
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .context("failed to serialize platform plugin state")?;
        if next_json == current_json {
            tx.commit()?;
            return Ok(next);
        }
        if let Some(value_json) = next_json {
            tx.execute(
                "INSERT INTO platform_plugin_kv (
                    plugin_id, platform, account_id, conversation_kind,
                    conversation_id, key, value_json, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (
                    plugin_id, platform, account_id, conversation_kind,
                    conversation_id, key
                 ) DO UPDATE SET
                    value_json = excluded.value_json,
                    updated_at = excluded.updated_at",
                params![
                    scope.plugin_id,
                    scope.platform,
                    scope.account_id,
                    scope.conversation_kind,
                    scope.conversation_id,
                    key,
                    value_json,
                    Utc::now().to_rfc3339(),
                ],
            )?;
        } else {
            tx.execute(
                "DELETE FROM platform_plugin_kv
                 WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
                   AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
                params![
                    scope.plugin_id,
                    scope.platform,
                    scope.account_id,
                    scope.conversation_kind,
                    scope.conversation_id,
                    key,
                ],
            )?;
        }
        tx.commit()?;
        Ok(next)
    }

    pub fn plugin_delete_key(&self, scope: &PlatformPluginScopeKey, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM platform_plugin_kv
             WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
               AND conversation_kind = ?4 AND conversation_id = ?5 AND key = ?6",
            params![
                scope.plugin_id,
                scope.platform,
                scope.account_id,
                scope.conversation_kind,
                scope.conversation_id,
                key,
            ],
        )?;
        Ok(deleted != 0)
    }

    pub fn plugin_delete_scope(&self, scope: &PlatformPluginScopeKey) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM platform_plugin_kv
             WHERE plugin_id = ?1 AND platform = ?2 AND account_id = ?3
               AND conversation_kind = ?4 AND conversation_id = ?5",
            params![
                scope.plugin_id,
                scope.platform,
                scope.account_id,
                scope.conversation_kind,
                scope.conversation_id,
            ],
        )?)
    }

    pub fn put_platform_meme_ref(&self, record: &PlatformMemeRefRecord) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO platform_meme_refs (
                platform, account_id, conversation_kind, conversation_id,
                message_id, library, meme_id, direction, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (
                platform, account_id, conversation_kind, conversation_id,
                message_id, library, meme_id
             ) DO UPDATE SET
                direction = excluded.direction,
                created_at = excluded.created_at",
            params![
                record.platform,
                record.account_id,
                record.conversation_kind,
                record.conversation_id,
                record.message_id,
                record.library,
                record.meme_id,
                record.direction,
                record.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn platform_meme_refs_for_message(
        &self,
        platform: &str,
        account_id: &str,
        conversation_kind: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<PlatformMemeRefRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT platform, account_id, conversation_kind, conversation_id,
                    message_id, library, meme_id, direction, created_at
             FROM platform_meme_refs
             WHERE platform = ?1 AND account_id = ?2
               AND conversation_kind = ?3 AND conversation_id = ?4
               AND message_id = ?5
             ORDER BY created_at ASC, library ASC, meme_id ASC",
        )?;
        let records = stmt
            .query_map(
                params![
                    platform,
                    account_id,
                    conversation_kind,
                    conversation_id,
                    message_id
                ],
                |row| {
                    Ok(PlatformMemeRefRecord {
                        platform: row.get(0)?,
                        account_id: row.get(1)?,
                        conversation_kind: row.get(2)?,
                        conversation_id: row.get(3)?,
                        message_id: row.get(4)?,
                        library: row.get(5)?,
                        meme_id: row.get(6)?,
                        direction: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// dashboard 用:某插件在某类会话下的全部 kv 行(好感度档案这种"每用户一行"的
    /// 键要按前缀筛,只能整批拉回来再挑)。
    pub fn plugin_rows(
        &self,
        plugin_id: &str,
        conversation_kind: &str,
    ) -> Result<Vec<PlatformPluginRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT platform, account_id, conversation_id, key, value_json, updated_at
               FROM platform_plugin_kv
              WHERE plugin_id = ?1 AND conversation_kind = ?2
              ORDER BY account_id, conversation_id, key",
        )?;
        let rows = stmt
            .query_map(params![plugin_id, conversation_kind], |row| {
                let updated: rusqlite::types::Value = row.get(5)?;
                Ok(PlatformPluginRow {
                    scope: PlatformPluginScopeKey {
                        plugin_id: plugin_id.to_string(),
                        platform: row.get(0)?,
                        account_id: row.get(1)?,
                        conversation_kind: conversation_kind.to_string(),
                        conversation_id: row.get(2)?,
                    },
                    key: row.get(3)?,
                    value_json: row.get(4)?,
                    updated_at: match updated {
                        rusqlite::types::Value::Integer(v) => v.to_string(),
                        rusqlite::types::Value::Real(v) => v.to_string(),
                        rusqlite::types::Value::Text(v) => v,
                        _ => String::new(),
                    },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// dashboard 用:某插件在哪些 (平台, 账号, 会话类型, 会话) 下留有记录。
    pub fn plugin_scopes(
        &self,
        plugin_id: &str,
        conversation_kind: Option<&str>,
    ) -> Result<Vec<PlatformPluginScopeKey>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT DISTINCT platform, account_id, conversation_kind, conversation_id
               FROM platform_plugin_kv WHERE plugin_id = ?1",
        );
        let mut args: Vec<String> = vec![plugin_id.to_string()];
        if let Some(kind) = conversation_kind {
            sql.push_str(" AND conversation_kind = ?2");
            args.push(kind.to_string());
        }
        sql.push_str(" ORDER BY platform, account_id, conversation_kind, conversation_id");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |row| {
                Ok(PlatformPluginScopeKey {
                    plugin_id: plugin_id.to_string(),
                    platform: row.get(0)?,
                    account_id: row.get(1)?,
                    conversation_kind: row.get(2)?,
                    conversation_id: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// dashboard 用:某个库里每张表情的入站/出站引用次数与最近一次时间。
    pub fn platform_meme_ref_counts(&self, library: &str) -> Result<Vec<PlatformMemeRefCount>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT meme_id,
                    SUM(CASE WHEN direction = 'inbound' THEN 1 ELSE 0 END),
                    SUM(CASE WHEN direction = 'outbound' THEN 1 ELSE 0 END),
                    MAX(created_at)
             FROM platform_meme_refs
             WHERE library = ?1
             GROUP BY meme_id",
        )?;
        let rows = stmt
            .query_map(params![library], |row| {
                Ok(PlatformMemeRefCount {
                    meme_id: row.get(0)?,
                    inbound: row.get(1)?,
                    outbound: row.get(2)?,
                    last_seen_at: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_platform_meme_ref(&self, library: &str, meme_id: &str) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM platform_meme_refs WHERE library = ?1 AND meme_id = ?2",
            params![library, meme_id],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Records the model identity and token usage a subagent session actually
    /// used (audit columns on `sessions`).
    /// Writes a subagent row the way builds before v19 did: usage present,
    /// `cache_read_tokens` left NULL.
    #[cfg(test)]
    pub fn record_legacy_subagent_usage_for_test(
        &self,
        session_id: &str,
        prompt_tokens: i64,
        total_tokens: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET prompt_tokens = ?2, total_tokens = ?3,
                    cache_read_tokens = NULL
             WHERE session_id = ?1",
            params![session_id, prompt_tokens, total_tokens],
        )?;
        Ok(())
    }

    pub fn record_subagent_usage(
        &self,
        session_id: &str,
        provider_id: Option<&str>,
        model: Option<&str>,
        context_window: Option<i64>,
        prompt_tokens: i64,
        completion_tokens: i64,
        total_tokens: i64,
        cache_read_tokens: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET provider_id = ?2, model = ?3, context_window = ?4,
                    prompt_tokens = ?5, completion_tokens = ?6, total_tokens = ?7,
                    updated_at = ?8, cache_read_tokens = ?9
             WHERE session_id = ?1",
            params![
                session_id,
                provider_id,
                model,
                context_window,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                Utc::now().to_rfc3339(),
                cache_read_tokens,
            ],
        )?;
        Ok(())
    }
}
