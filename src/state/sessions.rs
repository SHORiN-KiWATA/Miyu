//! 会话与人格归属。
//!
//! `StateStore` 这一层在库之上加的是**缓存与授权**：平台授权在内存里有索引
//! （见 [`crate::PlatformAccessIndex`]），因为每条群消息都要查一次，走 SQL 太贵。
//!
//! 授权变更必须先过 `with_platform_access_authorization`：改授权本身也是要授权
//! 的操作，不然任何人都能把自己加进白名单。

use crate::state::*;

impl StateStore {
    pub(crate) fn session(&self) -> Arc<str> {
        self.session_id.read().unwrap().clone()
    }

    /// Points this store (and every clone sharing it) at another session.
    /// The caller is responsible for persisting the current-session pointer.
    pub fn adopt_session(&self, session_id: &str) {
        *self.session_id.write().unwrap() = session_id.into();
    }

    /// Switches the active session and persists the current-session pointer.
    pub fn switch_session(&self, session_id: &str) -> Result<()> {
        self.conv_db.set_current_session(session_id)?;
        self.adopt_session(session_id);
        Ok(())
    }

    pub fn has_platform_access_grant(
        &self,
        platform: &str,
        account_id: &str,
        permission: &str,
        subject_kind: &str,
        subject_id: &str,
    ) -> bool {
        let access = self.platform_access.index.read().unwrap();
        access.contains(
            platform,
            GLOBAL_PLATFORM_ACCOUNT_SCOPE,
            permission,
            subject_kind,
            subject_id,
        ) || (account_id != GLOBAL_PLATFORM_ACCOUNT_SCOPE
            && access.contains(platform, account_id, permission, subject_kind, subject_id))
    }

    pub fn platform_access_grants(&self, platform: &str) -> Result<Vec<PlatformAccessGrant>> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        self.conv_db.platform_access_grants(Some(platform))
    }

    pub(crate) fn platform_access_grants_if_authorized(
        &self,
        platform: &str,
        authorization: &PlatformAccessAuthorization,
    ) -> Result<Option<Vec<PlatformAccessGrant>>> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        if !self.platform_access_authorized(authorization) {
            return Ok(None);
        }
        self.conv_db
            .platform_access_grants(Some(platform))
            .map(Some)
    }

    pub(crate) fn mutate_platform_access_grant_if_authorized(
        &self,
        key: &PlatformAccessGrantKey,
        actor: &PlatformAccessActor,
        operation: PlatformAccessMutation,
        authorization: &PlatformAccessAuthorization,
    ) -> Result<PlatformAccessMutationResult> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        if !self.platform_access_authorized(authorization) {
            return Ok(PlatformAccessMutationResult::Unauthorized);
        }
        match operation {
            PlatformAccessMutation::Grant => {
                let inserted = self.conv_db.add_platform_access_grant(key, actor)?;
                if inserted {
                    self.platform_access.index.write().unwrap().insert(key);
                    Ok(PlatformAccessMutationResult::Changed)
                } else {
                    Ok(PlatformAccessMutationResult::Unchanged)
                }
            }
            PlatformAccessMutation::Revoke => {
                let was_cached = self.platform_access.index.write().unwrap().remove(key);
                match self.conv_db.remove_platform_access_grant(key, actor) {
                    Ok(true) => Ok(PlatformAccessMutationResult::Changed),
                    Ok(false) => Ok(PlatformAccessMutationResult::Unchanged),
                    Err(error) => {
                        if was_cached {
                            self.platform_access.index.write().unwrap().insert(key);
                        }
                        Err(error)
                    }
                }
            }
        }
    }

    /// Runs an operation while holding the platform-access mutation lock.
    /// The callback must not call another access-control mutation method.
    pub(crate) fn with_platform_access_authorization<T>(
        &self,
        authorization: &PlatformAccessAuthorization,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        if !self.platform_access_authorized(authorization) {
            return Ok(None);
        }
        operation().map(Some)
    }

    pub fn add_platform_access_grant(
        &self,
        key: &PlatformAccessGrantKey,
        actor: &PlatformAccessActor,
    ) -> Result<bool> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        let inserted = self.conv_db.add_platform_access_grant(key, actor)?;
        if inserted {
            self.platform_access.index.write().unwrap().insert(key);
        }
        Ok(inserted)
    }

    pub fn remove_platform_access_grant(
        &self,
        key: &PlatformAccessGrantKey,
        actor: &PlatformAccessActor,
    ) -> Result<bool> {
        let _mutation = self.platform_access.mutations.lock().unwrap();
        let was_cached = self.platform_access.index.write().unwrap().remove(key);
        match self.conv_db.remove_platform_access_grant(key, actor) {
            Ok(deleted) => Ok(deleted),
            Err(error) => {
                if was_cached {
                    self.platform_access.index.write().unwrap().insert(key);
                }
                Err(error)
            }
        }
    }

    pub(crate) fn platform_access_authorized(
        &self,
        authorization: &PlatformAccessAuthorization,
    ) -> bool {
        if authorization.statically_authorized {
            return true;
        }
        let key = &authorization.dynamic_key;
        let access = self.platform_access.index.read().unwrap();
        access.contains(
            &key.platform,
            GLOBAL_PLATFORM_ACCOUNT_SCOPE,
            &key.permission,
            &key.subject_kind,
            &key.subject_id,
        ) || (key.account_scope != GLOBAL_PLATFORM_ACCOUNT_SCOPE
            && access.contains(
                &key.platform,
                &key.account_scope,
                &key.permission,
                &key.subject_kind,
                &key.subject_id,
            ))
    }

    pub fn persona_current_session(&self, persona: &str) -> Result<Option<String>> {
        self.conv_db.persona_current_session(persona)
    }

    pub fn set_persona_current_session(&self, persona: &str, session_id: &str) -> Result<()> {
        self.conv_db
            .set_persona_current_session(persona, session_id)
    }

    /// Session the REPL was last on, or `None` when that pointer is unset or
    /// stale (deleted, archived, or another persona's).
    pub fn repl_session(&self, persona: &str) -> Result<Option<String>> {
        self.conv_db.repl_session(persona)
    }

    pub fn set_repl_session(&self, persona: &str, session_id: &str) -> Result<()> {
        self.conv_db.set_repl_session(persona, session_id)
    }

    /// 这条人格车道的 REPL 会话；指针缺失就自举一条新的。
    ///
    /// 终端集成（shellhook）、普通 REPL、开发 REPL 是**三条并行车道**，各自
    /// 记一个指针。normal 以前在指针缺失时退到 `session_id()`——那是终端集成
    /// 那条车道，于是第一次 `miyu normal` 就把两边焊在同一个会话上，对话混成
    /// 一摊。dev 早就是自举的，normal 没跟上。
    ///
    /// 远端（`GetReplSession`）与直连（`run_direct_repl`）两条入口共用这里：
    /// 分开写过一次，改一边漏一边，行为就分叉了。
    pub fn ensure_repl_session(&self, persona: &str) -> Result<String> {
        match self.repl_session(persona)? {
            // 指针指向终端集成会话=视同缺失(08-25 用户裁定):normal 永远
            // 不自动进终端车道——历史上的 /session 切换把指针钉过去、或
            // 老回落语义留下的残值,都在这里自愈成一条新会话。
            Some(session_id) if session_id != crate::state::DEFAULT_SESSION_ID => Ok(session_id),
            _ => self.new_repl_session(persona),
        }
    }

    /// 给这条人格车道新建一个会话并钉住指针。
    ///
    /// 名字留空是有意的：首条消息会自动命名。不动 `session_id()`——终端车道
    /// 保持原样，用户要回去用 `/session`。
    pub fn new_repl_session(&self, persona: &str) -> Result<String> {
        let record = self.create_session(persona, "", crate::state::USER_SESSION_KIND, None)?;
        self.set_repl_session(persona, &record.session_id)?;
        Ok(record.session_id)
    }

    /// Claims persona-less sessions (schema-v2 migrated rows) for the active
    /// persona scope.
    pub fn adopt_sessions_for_persona(&self, persona: &str) -> Result<()> {
        self.conv_db.adopt_sessions_for_persona(persona)
    }

    pub fn rename_persona_scope(&self, old_scope: &str, new_scope: &str) -> Result<()> {
        self.conv_db.rename_persona_scope(old_scope, new_scope)
    }

    pub fn delete_persona_scope(&self, scope: &str) -> Result<()> {
        let session_ids = self
            .conv_db
            .list_sessions(scope)?
            .into_iter()
            .map(|session| session.record.session_id)
            .collect::<Vec<_>>();
        self.conv_db.delete_persona_scope(scope)?;
        self.remove_artifact_session_dirs(&session_ids)
    }

    pub fn session_record(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        self.conv_db.session_record(session_id)
    }

    pub fn list_sessions(&self, persona: &str) -> Result<Vec<SessionOverview>> {
        self.conv_db.list_sessions(persona)
    }

    pub fn list_local_sessions(&self, persona: &str) -> Result<Vec<SessionOverview>> {
        self.conv_db.list_local_sessions(persona)
    }

    pub fn is_platform_session(&self, session_id: &str) -> Result<bool> {
        self.conv_db.is_platform_session(session_id)
    }

    pub fn persona_reset_session_ids(&self, persona: &str, platform: &str) -> Result<Vec<String>> {
        self.conv_db.persona_reset_session_ids(persona, platform)
    }

    pub fn platform_session_bindings(
        &self,
        persona: &str,
        platform: &str,
    ) -> Result<Vec<PlatformSessionBinding>> {
        self.conv_db.platform_session_bindings(persona, platform)
    }

    pub fn create_session(
        &self,
        persona: &str,
        name: &str,
        kind: &str,
        parent_session_id: Option<&str>,
    ) -> Result<SessionRecord> {
        self.conv_db
            .create_session(persona, name, kind, parent_session_id)
    }

    pub fn create_or_get_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        name: &str,
    ) -> Result<(SessionRecord, bool)> {
        self.conv_db.create_or_get_platform_session(key, name)
    }

    pub fn rename_session(&self, session_id: &str, name: &str) -> Result<()> {
        self.conv_db.rename_session(session_id, name)
    }

    pub fn reorder_sessions(&self, ordered_ids: &[String]) -> Result<()> {
        self.conv_db.reorder_sessions(ordered_ids)
    }

    pub fn set_session_workspace(&self, session_id: &str, workspace: Option<&str>) -> Result<()> {
        self.conv_db.set_session_workspace(session_id, workspace)
    }

    /// Per-session model pool override. None follows the global active pool.
    pub fn session_model_override(
        &self,
        session_id: &str,
    ) -> Result<Option<Vec<crate::config::ActiveProviderModelConfig>>> {
        let Some(encoded) = self.conv_db.session_model_override(session_id)? else {
            return Ok(None);
        };
        let models =
            serde_json::from_str::<Vec<crate::config::ActiveProviderModelConfig>>(&encoded)
                .with_context(|| format!("invalid session model override for {session_id}"))?;
        Ok((!models.is_empty()).then_some(models))
    }

    pub fn set_session_model_override(
        &self,
        session_id: &str,
        models: Option<&[crate::config::ActiveProviderModelConfig]>,
    ) -> Result<()> {
        let encoded = match models {
            Some(models) if !models.is_empty() => Some(serde_json::to_string(models)?),
            _ => None,
        };
        self.conv_db
            .set_session_model_override(session_id, encoded.as_deref())
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.conv_db.delete_session(session_id)?;
        self.remove_artifact_session_dir(session_id)
    }

    pub fn touch_session(&self, session_id: &str) -> Result<()> {
        self.conv_db.touch_session(session_id)
    }

    pub fn find_session_by_name(&self, persona: &str, name: &str) -> Result<Option<SessionRecord>> {
        self.conv_db.find_session_by_name(persona, name)
    }

    pub fn find_local_session_by_name(
        &self,
        persona: &str,
        name: &str,
    ) -> Result<Option<SessionRecord>> {
        self.conv_db.find_local_session_by_name(persona, name)
    }

    pub fn find_platform_session_binding(
        &self,
        key: &PlatformSessionBindingKey,
    ) -> Result<Option<String>> {
        self.conv_db.find_platform_session_binding(key)
    }

    pub fn bind_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        session_id: &str,
    ) -> Result<()> {
        self.conv_db.bind_platform_session(key, session_id)
    }

    pub fn claim_platform_session(
        &self,
        key: &PlatformSessionBindingKey,
        candidate_session_id: &str,
    ) -> Result<String> {
        self.conv_db
            .claim_platform_session(key, candidate_session_id)
    }

    pub fn unbind_platform_session(&self, key: &PlatformSessionBindingKey) -> Result<bool> {
        self.conv_db.unbind_platform_session(key)
    }

    pub fn plugin_get_json<T: serde::de::DeserializeOwned>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<T>> {
        self.conv_db.plugin_get_json(scope, key)
    }

    pub(crate) fn plugin_json_revision(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<String>> {
        self.conv_db.plugin_json_revision(scope, key)
    }

    pub(crate) fn plugin_get_json_with_revision<T: serde::de::DeserializeOwned>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
    ) -> Result<Option<(T, String)>> {
        self.conv_db.plugin_get_json_with_revision(scope, key)
    }

    pub fn plugin_put_json<T: Serialize + ?Sized>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
        value: &T,
    ) -> Result<()> {
        self.conv_db.plugin_put_json(scope, key, value)
    }

    /// Atomically reads and replaces one platform-plugin JSON value.
    pub fn plugin_update_json<T, F>(
        &self,
        scope: &PlatformPluginScopeKey,
        key: &str,
        update: F,
    ) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned + Serialize,
        F: FnOnce(Option<T>) -> Result<Option<T>>,
    {
        self.conv_db.plugin_update_json(scope, key, update)
    }

    pub fn plugin_delete_key(&self, scope: &PlatformPluginScopeKey, key: &str) -> Result<bool> {
        self.conv_db.plugin_delete_key(scope, key)
    }

    pub fn plugin_delete_scope(&self, scope: &PlatformPluginScopeKey) -> Result<usize> {
        self.conv_db.plugin_delete_scope(scope)
    }

    pub fn put_platform_meme_ref(&self, record: &PlatformMemeRefRecord) -> Result<()> {
        self.conv_db.put_platform_meme_ref(record)
    }

    pub fn platform_meme_refs_for_message(
        &self,
        platform: &str,
        account_id: &str,
        conversation_kind: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<Vec<PlatformMemeRefRecord>> {
        self.conv_db.platform_meme_refs_for_message(
            platform,
            account_id,
            conversation_kind,
            conversation_id,
            message_id,
        )
    }

    pub fn delete_platform_meme_ref(&self, library: &str, meme_id: &str) -> Result<usize> {
        self.conv_db.delete_platform_meme_ref(library, meme_id)
    }

    pub fn plugin_scopes(
        &self,
        plugin_id: &str,
        conversation_kind: Option<&str>,
    ) -> Result<Vec<PlatformPluginScopeKey>> {
        self.conv_db.plugin_scopes(plugin_id, conversation_kind)
    }

    pub fn platform_meme_ref_counts(
        &self,
        library: &str,
    ) -> Result<Vec<crate::state::conversation_db::PlatformMemeRefCount>> {
        self.conv_db.platform_meme_ref_counts(library)
    }

    pub fn delete_subagent_sessions_older_than(&self, days: i64) -> Result<usize> {
        self.conv_db.delete_subagent_sessions_older_than(days)
    }

    pub fn delete_ask_sessions_older_than(&self, hours: i64) -> Result<usize> {
        self.conv_db.delete_ask_sessions_older_than(hours)
    }
}
