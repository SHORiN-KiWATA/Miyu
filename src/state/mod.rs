mod conversation_db;
mod migrations;
mod usage;

/// Newest `conversation.db` schema this build can open — the gate an import
/// checks before restoring a database written by a newer Miyu.
pub fn latest_schema_version() -> i64 {
    migrations::LATEST_VERSION
}

use crate::llm::{TurnTokens, Usage};
use crate::memory::EvictedTurn;
use crate::paths::MiyuPaths;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Cursor, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

#[allow(unused_imports)]
pub use conversation_db::{
    interrupted_text, pending_placeholder, ArtifactAsset, ArtifactAssetData, ConversationDb,
    ImageAsset, ImageAssetData, PlatformAccessActor, PlatformAccessGrant, PlatformAccessGrantKey,
    PlatformMemeRefRecord, PlatformPluginScopeKey, PlatformSessionBinding,
    PlatformSessionBindingKey, PruneStats, QueuedPrompt, QueuedPromptAttachment, RedoCandidate,
    RedoInputKind, RedoStart, ReplayEntry, SessionOverview, SessionRecord, ToolFootprint, Turn,
    TurnFollowup, TurnReplay,
    TurnJournalEvent,
    TurnRedoCheckpointPayload, TurnStatus, UserAttachment, UserAttachmentData,
    GLOBAL_PLATFORM_ACCOUNT_SCOPE,
};
pub use usage::UsageSnapshot;

/// The only session kind users can list, name, switch to, or bind a platform
/// to. Everything else is infrastructure and stays out of the session list.
pub const USER_SESSION_KIND: &str = "user";
/// Backs a one-shot `miyu ask` / `miyu '<message>'` turn: created just before
/// the turn, deleted right after, and invisible to every listing in between.
pub const ASK_SESSION_KIND: &str = "ask";

type PlatformAccessSubjects = HashSet<String>;
type PlatformAccessKinds = HashMap<String, PlatformAccessSubjects>;
type PlatformAccessPermissions = HashMap<String, PlatformAccessKinds>;
type PlatformAccessScopes = HashMap<String, PlatformAccessPermissions>;

#[derive(Debug)]
struct SharedPlatformAccess {
    index: RwLock<PlatformAccessIndex>,
    mutations: Mutex<()>,
}

static PLATFORM_ACCESS_INDEXES: OnceLock<Mutex<HashMap<PathBuf, Weak<SharedPlatformAccess>>>> =
    OnceLock::new();

#[derive(Debug, Default)]
struct PlatformAccessIndex {
    platforms: HashMap<String, PlatformAccessScopes>,
}

impl PlatformAccessIndex {
    fn from_grants(grants: impl IntoIterator<Item = PlatformAccessGrant>) -> Self {
        let mut index = Self::default();
        for grant in grants {
            index.insert(&grant.key);
        }
        index
    }

    fn contains(
        &self,
        platform: &str,
        account_scope: &str,
        permission: &str,
        subject_kind: &str,
        subject_id: &str,
    ) -> bool {
        self.platforms
            .get(platform)
            .and_then(|scopes| scopes.get(account_scope))
            .and_then(|permissions| permissions.get(permission))
            .and_then(|kinds| kinds.get(subject_kind))
            .is_some_and(|subjects| subjects.contains(subject_id))
    }

    fn insert(&mut self, key: &PlatformAccessGrantKey) {
        self.platforms
            .entry(key.platform.clone())
            .or_default()
            .entry(key.account_scope.clone())
            .or_default()
            .entry(key.permission.clone())
            .or_default()
            .entry(key.subject_kind.clone())
            .or_default()
            .insert(key.subject_id.clone());
    }

    fn remove(&mut self, key: &PlatformAccessGrantKey) -> bool {
        if let Some(subjects) = self
            .platforms
            .get_mut(&key.platform)
            .and_then(|scopes| scopes.get_mut(&key.account_scope))
            .and_then(|permissions| permissions.get_mut(&key.permission))
            .and_then(|kinds| kinds.get_mut(&key.subject_kind))
        {
            return subjects.remove(&key.subject_id);
        }
        false
    }
}

fn shared_platform_access_index(
    state_dir: &Path,
    conv_db: &ConversationDb,
) -> Result<Arc<SharedPlatformAccess>> {
    let key = state_dir
        .canonicalize()
        .unwrap_or_else(|_| state_dir.to_path_buf());
    let indexes = PLATFORM_ACCESS_INDEXES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut indexes = indexes.lock().unwrap();
    if let Some(index) = indexes.get(&key).and_then(Weak::upgrade) {
        return Ok(index);
    }
    indexes.retain(|_, index| index.strong_count() > 0);
    let index = Arc::new(SharedPlatformAccess {
        index: RwLock::new(PlatformAccessIndex::from_grants(
            conv_db.platform_access_grants(None)?,
        )),
        mutations: Mutex::new(()),
    });
    indexes.insert(key, Arc::downgrade(&index));
    Ok(index)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningTurnQueueTarget {
    pub turn_id: String,
    pub queue_session_id: Option<String>,
    pub owner_pid: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct PlatformAccessAuthorization {
    pub(crate) statically_authorized: bool,
    pub(crate) dynamic_key: PlatformAccessGrantKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlatformAccessMutation {
    Grant,
    Revoke,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlatformAccessMutationResult {
    Unauthorized,
    Unchanged,
    Changed,
}

#[derive(Debug, Clone)]
pub struct StateStore {
    state_dir: PathBuf,
    artifacts_dir: PathBuf,
    conv_db: Arc<ConversationDb>,
    platform_access: Arc<SharedPlatformAccess>,
    /// Active session. Shared across clones and swappable at runtime so a
    /// long-lived daemon switches every holder atomically.
    session_id: Arc<std::sync::RwLock<Arc<str>>>,
    queue_session_id: Arc<str>,
    queue_owner_pid: u32,
}

impl StateStore {
    pub fn new(paths: &MiyuPaths) -> Result<Self> {
        let state_dir = paths.state_dir.clone();
        let conv_db = Arc::new(ConversationDb::open(&state_dir)?);
        let platform_access = shared_platform_access_index(&state_dir, &conv_db)?;
        let session_id = Arc::new(std::sync::RwLock::new(Arc::<str>::from(
            conv_db.resolve_current_session()?,
        )));
        let queue_owner_pid = std::process::id();
        let queue_session_id: Arc<str> = format!(
            "queue_{}_{}_{}",
            queue_owner_pid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            rand::random::<u64>()
        )
        .into();
        conv_db.discard_stale_queued_prompts(&queue_session_id, queue_owner_pid)?;
        Ok(Self {
            state_dir,
            artifacts_dir: paths.data_dir.join("artifacts"),
            conv_db,
            platform_access,
            session_id,
            queue_session_id,
            queue_owner_pid,
        })
    }

    pub fn session_id(&self) -> Arc<str> {
        self.session_id.read().unwrap().clone()
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    fn session(&self) -> Arc<str> {
        self.session_id.read().unwrap().clone()
    }

    /// Points this store (and every clone sharing it) at another session.
    /// The caller is responsible for persisting the current-session pointer.
    pub fn adopt_session(&self, session_id: &str) {
        *self.session_id.write().unwrap() = session_id.into();
    }

    /// A clone pinned to the given session: it shares the database but holds
    /// its own session pointer, unaffected by later `switch_session` /
    /// `adopt_session` calls on other clones. Used by concurrently running
    /// turns so each keeps writing to the session it started in.
    pub fn pinned(&self, session_id: &str) -> Self {
        Self {
            state_dir: self.state_dir.clone(),
            artifacts_dir: self.artifacts_dir.clone(),
            conv_db: self.conv_db.clone(),
            platform_access: self.platform_access.clone(),
            session_id: Arc::new(std::sync::RwLock::new(session_id.into())),
            queue_session_id: self.queue_session_id.clone(),
            queue_owner_pid: self.queue_owner_pid,
        }
    }

    /// Like [`pinned`], but with a fresh queue identity so concurrently
    /// running turns in the same session never consume each other's queued
    /// follow-up prompts. Callers should `discard_queued_prompts()` when the
    /// turn finishes.
    pub fn pinned_for_turn(&self, session_id: &str) -> Self {
        let mut store = self.pinned(session_id);
        store.queue_session_id = format!(
            "queue_{}_{}_{}",
            store.queue_owner_pid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            rand::random::<u64>()
        )
        .into();
        store
    }

    pub(crate) fn queue_target(&self, turn_id: impl Into<String>) -> RunningTurnQueueTarget {
        RunningTurnQueueTarget {
            turn_id: turn_id.into(),
            queue_session_id: Some(self.queue_session_id.to_string()),
            owner_pid: Some(self.queue_owner_pid),
        }
    }

    /// Whether any session has a running turn (global admin guard).
    pub fn has_any_running_turns(&self) -> Result<bool> {
        self.conv_db.has_any_running_turns()
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

    fn platform_access_authorized(&self, authorization: &PlatformAccessAuthorization) -> bool {
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
            .list_sessions(scope, true)?
            .into_iter()
            .map(|session| session.record.session_id)
            .collect::<Vec<_>>();
        self.conv_db.delete_persona_scope(scope)?;
        self.remove_artifact_session_dirs(&session_ids)
    }

    pub fn session_record(&self, session_id: &str) -> Result<Option<SessionRecord>> {
        self.conv_db.session_record(session_id)
    }

    pub fn list_sessions(
        &self,
        persona: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionOverview>> {
        self.conv_db.list_sessions(persona, include_archived)
    }

    pub fn list_local_sessions(
        &self,
        persona: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionOverview>> {
        self.conv_db.list_local_sessions(persona, include_archived)
    }

    pub fn background_report_replies_after(
        &self,
        session_id: &str,
        after_seq: i64,
    ) -> Result<Vec<(i64, String, String, String)>> {
        self.conv_db
            .background_report_replies_after(session_id, after_seq)
    }

    pub fn latest_turn_seq(&self, session_id: &str) -> Result<i64> {
        self.conv_db.latest_turn_seq(session_id)
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

    pub fn set_session_archived(&self, session_id: &str, archived: bool) -> Result<()> {
        self.conv_db.set_session_archived(session_id, archived)
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

    #[allow(clippy::too_many_arguments)]
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
        self.conv_db.record_subagent_usage(
            session_id,
            provider_id,
            model,
            context_window,
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cache_read_tokens,
        )
    }

    pub fn delete_subagent_sessions_older_than(&self, days: i64) -> Result<usize> {
        self.conv_db.delete_subagent_sessions_older_than(days)
    }

    pub fn delete_ask_sessions_older_than(&self, hours: i64) -> Result<usize> {
        self.conv_db.delete_ask_sessions_older_than(hours)
    }

    pub fn init_files(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_dir)?;
        if !self.usage_file().exists() {
            std::fs::write(self.usage_file(), "{\n  \"requests\": 0,\n  \"prompt_tokens\": 0,\n  \"completion_tokens\": 0,\n  \"total_tokens\": 0,\n  \"conversation_tokens\": 0\n}\n")?;
        }
        if !self.profile_file().exists() {
            std::fs::write(self.profile_file(), "# Miyu Profile\n\n")?;
        }
        Ok(())
    }

    pub fn reset_if_prompt_changed(&self, system_prompt: &str) -> Result<()> {
        self.reset_if_prompt_changed_with_compatible(system_prompt, None)
    }

    pub(crate) fn reset_if_prompt_changed_with_compatible(
        &self,
        system_prompt: &str,
        // Kept for call-site compatibility; since the v7 no-delete semantics
        // every previous prompt is effectively compatible.
        _compatible_previous_prompt: Option<&str>,
    ) -> Result<()> {
        self.init_files()?;
        let fingerprint = prompt_fingerprint(system_prompt);
        let file = self.prompt_fingerprint_file();
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !file.exists() && self.state_dir.join("prompt.sha256").exists() {
            std::fs::write(file, format!("{fingerprint}\n"))?;
            return Ok(());
        }
        let previous = std::fs::read_to_string(&file).unwrap_or_default();
        if previous.trim() != fingerprint {
            // v7 Release 3: a persona prompt text change is a planned cache
            // cold start, not a reason to destroy data. Earlier versions
            // physically deleted every turn and the session's artifacts here,
            // which meant "upgrade the binary → conversations silently
            // vanish". History and artifacts are kept; only the fingerprint
            // advances. Users who want a clean slate still have /clear.
            tracing::info!(
                "persona prompt fingerprint changed; keeping session history (cache cold start)"
            );
            self.clear_last_usage()?;
            std::fs::write(file, format!("{fingerprint}\n"))?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn conv_db(&self) -> &ConversationDb {
        &self.conv_db
    }

    pub fn start_turn(&self, turn_id: &str, user_content: &str, owner_pid: u32) -> Result<()> {
        self.start_turn_with_display(turn_id, user_content, user_content, owner_pid, None)
    }

    pub fn start_turn_with_display(
        &self,
        turn_id: &str,
        user_content: &str,
        display_content: &str,
        owner_pid: u32,
        attachment_run_id: Option<&str>,
    ) -> Result<()> {
        // Record the ambient turn workspace (if any) so the turn row captures
        // where its tools operated; NULL outside a turn workspace scope.
        let workspace =
            crate::tools::workspace::try_workspace().map(|path| path.display().to_string());
        self.conv_db.start_turn(
            &self.session(),
            turn_id,
            user_content,
            display_content,
            owner_pid,
            &self.queue_session_id,
            workspace.as_deref(),
            attachment_run_id,
        )
    }

    #[allow(dead_code)]
    pub fn complete_turn(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
    ) -> Result<()> {
        self.conv_db.complete_turn(turn_id, content, reasoning)
    }

    pub fn interrupt_turn(&self, turn_id: &str) -> Result<()> {
        self.conv_db.interrupt_turn(turn_id)?;
        let session_id = self.session_id();
        self.recover_journal_assets(&session_id, turn_id)
    }

    pub fn interrupt_turn_revision(&self, turn_id: &str, revision: i64) -> Result<()> {
        let restored = self.conv_db.interrupt_turn_revision(turn_id, revision)?;
        if restored {
            let session_id = self
                .conv_db
                .turn_session_id(turn_id)?
                .context("restored redo turn no longer exists")?;
            self.reconcile_managed_artifacts_for_turn(&session_id, turn_id)?;
        } else {
            let session_id = self
                .conv_db
                .turn_session_id(turn_id)?
                .context("interrupted turn no longer exists")?;
            self.recover_journal_assets(&session_id, turn_id)?;
        }
        Ok(())
    }

    pub fn complete_turn_with_usage_and_model(
        &self,
        turn_id: &str,
        content: &str,
        reasoning: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db.complete_turn_with_usage(
            turn_id,
            content,
            reasoning,
            provider_id,
            model,
            tokens,
            token_usage_estimated,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn complete_turn_revision_with_usage_and_model(
        &self,
        turn_id: &str,
        revision: i64,
        content: &str,
        reasoning: Option<&str>,
        provider_id: Option<&str>,
        model: Option<&str>,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db.complete_turn_revision_with_usage(
            turn_id,
            revision,
            content,
            reasoning,
            provider_id,
            model,
            tokens,
            token_usage_estimated,
        )
    }

    pub fn append_persisted_context(&self, turn_id: &str, report: &str) -> Result<()> {
        self.conv_db.append_tool_report(turn_id, report.trim())
    }

    pub fn append_persisted_contexts(&self, turn_id: &str, reports: &[String]) -> Result<()> {
        self.conv_db.append_tool_reports(turn_id, reports)
    }

    /// Archives the transient system tail that was sent after the user message
    /// of this turn (v7 append-only fossilization). Replayed verbatim by
    /// history rendering so the byte stream stays a pure extension.
    pub fn set_turn_context_messages(
        &self,
        turn_id: &str,
        messages: &[crate::llm::ChatMessage],
    ) -> Result<()> {
        self.conv_db.set_turn_context_messages(turn_id, messages)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_turn_journal_event(
        &self,
        turn_id: &str,
        revision: i64,
        segment_index: i64,
        kind: &str,
        call_id: Option<&str>,
        name: Option<&str>,
        text_payload: Option<&str>,
        blob_payload: Option<&[u8]>,
        ok: Option<bool>,
    ) -> Result<()> {
        self.conv_db.append_turn_journal_event(
            turn_id,
            revision,
            segment_index,
            kind,
            call_id,
            name,
            text_payload,
            blob_payload,
            ok,
        )
    }

    pub fn supersede_turn_journal_segment(
        &self,
        turn_id: &str,
        revision: i64,
        segment_index: i64,
    ) -> Result<()> {
        self.conv_db
            .supersede_turn_journal_segment(turn_id, revision, segment_index)
    }

    pub fn save_image_asset(
        &self,
        turn_id: &str,
        tool_id: Option<&str>,
        path: &Path,
        alt: &str,
    ) -> Result<ImageAsset> {
        const MAX_STORED_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("reading image metadata: {}", path.display()))?;
        if !metadata.is_file() {
            bail!("image path is not a file: {}", path.display());
        }
        if metadata.len() > MAX_STORED_IMAGE_BYTES {
            bail!("image exceeds the 20 MiB WebUI storage limit");
        }
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading image for WebUI: {}", path.display()))?;
        let reader = image::ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .context("detecting image format")?;
        let format = reader.format().context("unsupported image format")?;
        let (width, height) = reader
            .into_dimensions()
            .context("reading image dimensions")?;
        if width == 0
            || height == 0
            || width > 40_000
            || height > 40_000
            || u64::from(width) * u64::from(height) > 40_000_000
        {
            bail!("image dimensions are outside the WebUI safety limit");
        }
        let fallback_alt = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image");
        let alt = if alt.trim().is_empty() {
            fallback_alt
        } else {
            alt.trim()
        }
        .chars()
        .filter(|character| !character.is_control())
        .take(500)
        .collect::<String>();
        let asset = ImageAsset {
            asset_id: format!(
                "img_{:032x}_{:016x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_nanos())
                    .unwrap_or(0),
                rand::random::<u64>()
            ),
            turn_id: turn_id.to_string(),
            tool_id: tool_id.map(str::to_string),
            mime: format.to_mime_type().to_string(),
            width,
            height,
            alt,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        self.conv_db.insert_image_asset(&asset, &bytes)?;
        Ok(asset)
    }

    pub fn load_image_assets(&self) -> Result<Vec<ImageAsset>> {
        self.conv_db.load_image_assets(&self.session())
    }

    pub fn load_image_asset(&self, asset_id: &str) -> Result<Option<ImageAssetData>> {
        self.conv_db.load_image_asset(asset_id)
    }

    pub fn save_artifact_asset(
        &self,
        turn_id: &str,
        tool_id: Option<&str>,
        path: &Path,
        title: &str,
    ) -> Result<ArtifactAsset> {
        const MAX_ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;
        let canonical = path
            .canonicalize()
            .with_context(|| format!("resolving artifact path: {}", path.display()))?;
        let metadata = std::fs::metadata(&canonical)
            .with_context(|| format!("reading artifact metadata: {}", canonical.display()))?;
        if !metadata.is_file() {
            bail!("artifact path is not a file: {}", canonical.display());
        }
        if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
            bail!("artifact must be between 1 byte and 20 MiB");
        }
        let bytes = std::fs::read(&canonical)
            .with_context(|| format!("reading artifact: {}", canonical.display()))?;
        let fallback_name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("artifact");
        let requested_name = if title.trim().is_empty() {
            fallback_name
        } else {
            title.trim()
        };
        let file_name = requested_name
            .chars()
            .filter(|character| !character.is_control() && !matches!(character, '/' | '\\'))
            .take(180)
            .collect::<String>();
        let file_name = if file_name.trim().is_empty() {
            "artifact".to_string()
        } else {
            file_name
        };
        let (mime, kind) = artifact_media_type(&canonical);
        let source_key = canonical.to_string_lossy().to_string();
        let session_id = self.session();
        let managed_session_dir = self.artifacts_dir.join(session_id.as_ref());
        let identity_scope = if canonical.starts_with(&managed_session_dir) {
            session_id.as_ref()
        } else {
            turn_id
        };
        let hash = blake3::hash(format!("{identity_scope}\0{source_key}").as_bytes());
        let now = chrono::Utc::now().to_rfc3339();
        let asset = ArtifactAsset {
            asset_id: format!("art_{}", &hash.to_hex()[..32]),
            turn_id: turn_id.to_string(),
            tool_id: tool_id.map(str::to_string),
            source_key,
            file_name,
            mime: mime.to_string(),
            kind: kind.to_string(),
            size_bytes: bytes.len() as u64,
            created_at: now.clone(),
            updated_at: now,
        };
        self.conv_db.upsert_artifact_asset(&asset, &bytes)?;
        Ok(asset)
    }

    pub fn load_artifact_assets(&self) -> Result<Vec<ArtifactAsset>> {
        self.conv_db.load_artifact_assets(&self.session())
    }

    pub fn load_artifact_asset(&self, asset_id: &str) -> Result<Option<ArtifactAssetData>> {
        self.conv_db.load_artifact_asset(asset_id)
    }

    pub fn save_user_attachment(&self, attachment: &UserAttachment, data: &[u8]) -> Result<()> {
        self.conv_db
            .insert_user_attachment(&self.session(), attachment, data)
    }

    pub fn load_user_attachment(&self, attachment_id: &str) -> Result<Option<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment(&self.session(), attachment_id)
    }

    pub fn load_user_attachment_by_id(
        &self,
        attachment_id: &str,
    ) -> Result<Option<UserAttachmentData>> {
        self.conv_db.load_user_attachment_by_id(attachment_id)
    }

    pub fn load_user_attachment_data_for_turn(
        &self,
        turn_id: &str,
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment_data_for_turn(&self.session(), turn_id)
    }

    pub fn load_user_attachment_data_for_prompt(
        &self,
        prompt_id: &str,
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachment_data_for_prompt(&self.session(), prompt_id)
    }

    pub fn load_staged_user_attachments(
        &self,
        attachment_ids: &[String],
    ) -> Result<Vec<UserAttachmentData>> {
        self.conv_db
            .load_user_attachments(&self.session(), attachment_ids)
    }

    pub fn reserve_user_attachments(&self, attachment_ids: &[String], run_id: &str) -> Result<()> {
        self.conv_db
            .reserve_user_attachments(&self.session(), attachment_ids, run_id)
    }

    pub fn release_user_attachments_for_run(&self, run_id: &str) -> Result<usize> {
        self.conv_db.release_user_attachments_for_run(run_id)
    }

    pub fn delete_staged_user_attachment(&self, attachment_id: &str) -> Result<bool> {
        self.conv_db
            .delete_staged_user_attachment(&self.session(), attachment_id)
    }

    pub fn purge_stale_user_attachments(&self) -> Result<usize> {
        self.conv_db.purge_stale_user_attachments()
    }

    pub fn append_question_exchange(
        &self,
        turn_id: &str,
        exchange: &crate::question::QuestionExchange,
    ) -> Result<()> {
        self.conv_db.append_question_exchange(turn_id, exchange)
    }

    pub fn enqueue_prompt(
        &self,
        prompt_id: &str,
        content: &str,
        display_content: &str,
        attachments: &[QueuedPromptAttachment],
    ) -> Result<QueuedPrompt> {
        self.enqueue_prompt_with_uploads(prompt_id, content, display_content, attachments, &[])
    }

    pub fn enqueue_prompt_with_uploads(
        &self,
        prompt_id: &str,
        content: &str,
        display_content: &str,
        attachments: &[QueuedPromptAttachment],
        uploaded_attachment_ids: &[String],
    ) -> Result<QueuedPrompt> {
        self.conv_db.enqueue_prompt(
            &self.session(),
            None,
            prompt_id,
            content,
            display_content,
            attachments,
            uploaded_attachment_ids,
            &self.queue_session_id,
            self.queue_owner_pid,
        )
    }

    pub fn running_turn_queue_target(&self) -> Result<Option<RunningTurnQueueTarget>> {
        Ok(self
            .conv_db
            .running_turn_queue_target(&self.session())?
            .map(
                |(turn_id, queue_session_id, owner_pid)| RunningTurnQueueTarget {
                    turn_id,
                    queue_session_id,
                    owner_pid,
                },
            ))
    }

    pub fn enqueue_prompt_for_target(
        &self,
        target: &RunningTurnQueueTarget,
        prompt_id: &str,
        content: &str,
        display_content: &str,
        attachments: &[QueuedPromptAttachment],
    ) -> Result<QueuedPrompt> {
        self.enqueue_prompt_for_target_with_uploads(
            target,
            prompt_id,
            content,
            display_content,
            attachments,
            &[],
        )
    }

    pub fn enqueue_prompt_for_target_with_uploads(
        &self,
        target: &RunningTurnQueueTarget,
        prompt_id: &str,
        content: &str,
        display_content: &str,
        attachments: &[QueuedPromptAttachment],
        uploaded_attachment_ids: &[String],
    ) -> Result<QueuedPrompt> {
        let queue_session_id = target
            .queue_session_id
            .as_deref()
            .context("running turn does not expose a queue session")?;
        let owner_pid = target
            .owner_pid
            .context("running turn does not expose an owner process")?;
        self.conv_db.enqueue_prompt(
            &self.session(),
            Some(&target.turn_id),
            prompt_id,
            content,
            display_content,
            attachments,
            uploaded_attachment_ids,
            queue_session_id,
            owner_pid,
        )
    }

    pub fn load_queued_prompts_for_target(
        &self,
        target: &RunningTurnQueueTarget,
    ) -> Result<Vec<QueuedPrompt>> {
        let Some(queue_session_id) = target.queue_session_id.as_deref() else {
            return Ok(Vec::new());
        };
        self.conv_db
            .load_queued_prompts(&self.session(), queue_session_id)
    }

    pub fn remove_queued_prompt_for_target(
        &self,
        target: &RunningTurnQueueTarget,
        prompt_id: &str,
    ) -> Result<bool> {
        let Some(queue_session_id) = target.queue_session_id.as_deref() else {
            return Ok(false);
        };
        self.conv_db
            .remove_queued_prompt(&self.session(), prompt_id, queue_session_id)
    }

    pub fn load_queued_prompts(&self) -> Result<Vec<QueuedPrompt>> {
        self.conv_db
            .load_queued_prompts(&self.session(), &self.queue_session_id)
    }

    #[cfg(test)]
    pub fn consume_queued_prompts(
        &self,
        turn_id: &str,
        prompts: &[(String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
    ) -> Result<()> {
        self.conv_db.consume_queued_prompts(
            &self.session(),
            turn_id,
            prompts,
            preceding_assistant_content,
            preceding_assistant_reasoning,
            None,
            None,
            &self.queue_session_id,
        )
    }

    pub fn consume_queued_prompts_with_model(
        &self,
        turn_id: &str,
        prompts: &[(String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
        preceding_assistant_provider_id: Option<&str>,
        preceding_assistant_model: Option<&str>,
    ) -> Result<()> {
        self.conv_db.consume_queued_prompts(
            &self.session(),
            turn_id,
            prompts,
            preceding_assistant_content,
            preceding_assistant_reasoning,
            preceding_assistant_provider_id,
            preceding_assistant_model,
            &self.queue_session_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn consume_queued_prompts_with_checkpoint(
        &self,
        turn_id: &str,
        prompts: &[(String, String)],
        preceding_assistant_content: Option<&str>,
        preceding_assistant_reasoning: Option<&str>,
        preceding_assistant_provider_id: Option<&str>,
        preceding_assistant_model: Option<&str>,
        checkpoint: TurnRedoCheckpointPayload,
    ) -> Result<()> {
        self.conv_db.consume_queued_prompts_with_checkpoint(
            &self.session(),
            turn_id,
            prompts,
            preceding_assistant_content,
            preceding_assistant_reasoning,
            preceding_assistant_provider_id,
            preceding_assistant_model,
            &self.queue_session_id,
            Some(checkpoint),
        )
    }

    /// Explicit-cancel variant of queue cleanup: drop still-queued prompts
    /// outright (no fold into context) and return the dropped ids.
    pub fn delete_queued_prompts(&self) -> Result<Vec<String>> {
        self.conv_db
            .delete_queued_prompts(&self.session(), &self.queue_session_id)
    }

    pub fn discard_queued_prompts(&self) -> Result<usize> {
        self.conv_db
            .discard_queued_prompts(&self.session(), &self.queue_session_id)
    }

    pub fn remove_queued_prompt(&self, prompt_id: &str) -> Result<bool> {
        self.conv_db
            .remove_queued_prompt(&self.session(), prompt_id, &self.queue_session_id)
    }

    pub fn load_session_loaded_tools(&self) -> Result<BTreeSet<String>> {
        self.conv_db
            .load_session_loaded_items(&self.session(), "tool")
    }

    pub fn load_session_loaded_tools_with_sources(&self) -> Result<Vec<(String, Option<String>)>> {
        self.conv_db
            .load_session_loaded_items_with_sources(&self.session(), "tool")
    }

    pub fn add_session_loaded_tools(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items(&self.session(), "tool", names, source_turn_id)?;
        Ok(())
    }

    pub fn add_session_loaded_targets(
        &self,
        names: &[String],
        source_turn_id: Option<&str>,
    ) -> Result<()> {
        self.conv_db
            .add_session_loaded_items(&self.session(), "target", names, source_turn_id)?;
        Ok(())
    }

    pub fn recover_stale_turns(&self) -> Result<usize> {
        let recoveries = self.conv_db.recover_stale_running_turns()?;
        for recovery in &recoveries {
            if recovery.restored_redo {
                self.reconcile_managed_artifacts_for_turn(&recovery.session_id, &recovery.turn_id)?;
            } else {
                self.recover_journal_assets(&recovery.session_id, &recovery.turn_id)?;
            }
        }
        Ok(recoveries.len())
    }

    pub fn history(&self, limit: usize) -> Result<Vec<StoredConversationEntry>> {
        let turns = self
            .conv_db
            .load_turns(&self.session())?
            .into_iter()
            .filter(|turn| !turn.is_summary)
            .collect();
        let mut entries = turns_to_entries(turns);
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    pub fn load_conversation(&self) -> Result<Vec<StoredConversationEntry>> {
        let turns = self
            .conv_db
            .load_turns(&self.session())?
            .into_iter()
            .filter(|turn| !turn.is_summary)
            .collect();
        Ok(turns_to_entries(turns))
    }

    #[allow(dead_code)]
    pub fn load_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_turns(&self.session())
    }

    #[allow(dead_code)]
    pub fn load_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db
            .load_turns_excluding(&self.session(), exclude_turn_id)
    }

    pub fn load_visible_turns(&self) -> Result<Vec<Turn>> {
        self.conv_db.load_visible_turns(&self.session())
    }

    /// Display transcripts of this session's last `limit` turns, for redrawing
    /// a reopened REPL.
    pub fn session_replay(&self, limit: usize) -> Result<Vec<conversation_db::TurnReplay>> {
        self.conv_db.session_replay(&self.session(), limit)
    }

    pub fn load_visible_turns_excluding(&self, exclude_turn_id: &str) -> Result<Vec<Turn>> {
        self.conv_db
            .load_visible_turns_excluding(&self.session(), exclude_turn_id)
    }

    #[allow(dead_code)]
    pub fn hide_turns_before_seq(&self, seq: i64) -> Result<usize> {
        self.conv_db.hide_turns_before_seq(&self.session(), seq)
    }

    #[allow(dead_code)]
    pub fn insert_summary_turn(
        &self,
        summary: &str,
        tokens: TurnTokens,
        token_usage_estimated: bool,
    ) -> Result<()> {
        self.conv_db
            .insert_summary_turn(&self.session(), summary, tokens, token_usage_estimated)
    }

    pub fn load_last_summary(&self) -> Result<Option<Turn>> {
        self.conv_db.load_last_summary(&self.session())
    }

    pub fn prune_stale_tool_reports(
        &self,
        protect_recent: usize,
        min_saved_chars: usize,
    ) -> Result<PruneStats> {
        self.conv_db
            .prune_stale_tool_reports(&self.session(), protect_recent, min_saved_chars)
    }

    pub fn session_last_request_at(&self) -> Result<Option<i64>> {
        self.conv_db.session_last_request_at(&self.session())
    }

    pub fn replace_visible_with_summary(
        &self,
        fold_turn_ids: &[String],
        visible_turn_ids: &[String],
        summary: &str,
        tokens: TurnTokens,
        token_usage_estimated: bool,
        footprint_json: Option<&str>,
    ) -> Result<()> {
        self.conv_db.replace_visible_with_summary(
            &self.session(),
            fold_turn_ids,
            visible_turn_ids,
            summary,
            tokens,
            token_usage_estimated,
            footprint_json,
        )
    }

    pub fn merge_turn_footprint(
        &self,
        turn_id: &str,
        delta: &crate::state::ToolFootprint,
    ) -> Result<()> {
        self.conv_db.merge_turn_footprint(turn_id, delta)
    }

    pub fn load_merged_footprint(
        &self,
        turn_ids: &[String],
    ) -> Result<crate::state::ToolFootprint> {
        self.conv_db.load_merged_footprint(&self.session(), turn_ids)
    }

    pub fn oldest_evictable_visible_turns(&self, count: usize) -> Result<Vec<Turn>> {
        self.conv_db
            .oldest_evictable_visible_turns(&self.session(), count)
    }

    pub fn delete_visible_turns(&self, turn_ids: &[String]) -> Result<usize> {
        self.conv_db.delete_visible_turns(&self.session(), turn_ids)
    }

    pub fn delete_visible_turns_checked(
        &self,
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db
            .delete_visible_turns_checked(&self.session(), turn_ids, expected_loaded_tools)
    }

    pub fn archive_and_delete_visible_turns(
        &self,
        archive_db: &Path,
        turns: &[EvictedTurn],
        turn_ids: &[String],
        expected_loaded_tools: Option<&[(String, Option<String>)]>,
    ) -> Result<usize> {
        self.conv_db.archive_and_delete_visible_turns(
            &self.session(),
            archive_db,
            turns,
            turn_ids,
            expected_loaded_tools,
        )
    }

    pub fn reset_conversation(&self) -> Result<()> {
        self.clear_session_content()?;
        usage::reset_conversation(&self.usage_file())
    }

    pub fn reset_conversation_usage(&self) -> Result<()> {
        usage::reset_conversation(&self.usage_file())
    }

    pub fn reset_persona_contexts(&self, persona: &str, platform: &str) -> Result<Vec<String>> {
        let session_ids = self.conv_db.reset_persona_contexts(persona, platform)?;
        self.remove_artifact_session_dirs(&session_ids)?;
        Ok(session_ids)
    }

    /// Clears only the pinned session's conversation state. Platform commands
    /// use this instead of `reset_conversation` so they cannot reset the
    /// daemon-wide usage counters or another client's current session.
    pub fn clear_session_content(&self) -> Result<()> {
        let session_id = self.session();
        self.conv_db.reset(&session_id)?;
        self.remove_artifact_session_dir(&session_id)
    }

    fn remove_artifact_session_dirs(&self, session_ids: &[String]) -> Result<()> {
        for session_id in session_ids {
            self.remove_artifact_session_dir(session_id)?;
        }
        Ok(())
    }

    fn remove_artifact_session_dir(&self, session_id: &str) -> Result<()> {
        use std::path::Component;

        let mut components = Path::new(session_id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            anyhow::bail!("invalid session id for Artifact workspace cleanup");
        }
        let path = self.artifacts_dir.join(session_id);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            std::fs::remove_file(path)?;
        } else {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn recover_journal_assets(&self, session_id: &str, turn_id: &str) -> Result<()> {
        let Some(turn) = self
            .conv_db
            .load_turns(session_id)?
            .into_iter()
            .find(|turn| turn.turn_id == turn_id)
        else {
            return Ok(());
        };
        if turn.journal_events.is_empty() {
            return Ok(());
        }
        let mut images = self
            .conv_db
            .load_image_assets(session_id)?
            .into_iter()
            .filter(|asset| asset.turn_id == turn_id)
            .collect::<Vec<_>>();
        let mut artifacts = self
            .conv_db
            .load_artifact_assets(session_id)?
            .into_iter()
            .filter(|asset| asset.turn_id == turn_id)
            .collect::<Vec<_>>();
        for event in &turn.journal_events {
            let kind = event.kind.as_str();
            if kind != "image" && kind != "artifact" {
                continue;
            }
            let Some(payload) = event
                .text_payload
                .as_deref()
                .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            else {
                continue;
            };
            let Some(raw_path) = payload.get("path").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let path = PathBuf::from(raw_path);
            if !path.is_file() {
                continue;
            }
            if kind == "image" {
                let alt = payload
                    .get("alt")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if images.iter().any(|asset| {
                    asset.tool_id.as_deref() == event.call_id.as_deref() && asset.alt == alt
                }) {
                    continue;
                }
                match self.save_image_asset(turn_id, event.call_id.as_deref(), &path, alt) {
                    Ok(asset) => images.push(asset),
                    Err(error) => tracing::warn!(
                        turn_id,
                        path = %path.display(),
                        error = %error,
                        "failed to recover an interrupted image asset"
                    ),
                }
            } else {
                let Ok(source_key) = path
                    .canonicalize()
                    .map(|path| path.to_string_lossy().into_owned())
                else {
                    continue;
                };
                if artifacts.iter().any(|asset| asset.source_key == source_key) {
                    continue;
                }
                let title = payload
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                match self.save_artifact_asset(turn_id, event.call_id.as_deref(), &path, title) {
                    Ok(asset) => artifacts.push(asset),
                    Err(error) => tracing::warn!(
                        turn_id,
                        path = %path.display(),
                        error = %error,
                        "failed to recover an interrupted Artifact asset"
                    ),
                }
            }
        }
        Ok(())
    }

    fn reconcile_managed_artifacts_for_turn(&self, session_id: &str, turn_id: &str) -> Result<()> {
        use std::path::Component;

        let mut components = Path::new(session_id).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            bail!("invalid session id for Artifact workspace recovery");
        }
        let restored = self.conv_db.load_artifact_asset_data_for_turn(turn_id)?;
        let session_dir = self.artifacts_dir.join(session_id);
        match std::fs::symlink_metadata(&session_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!(
                    "Artifact recovery path is not a directory: {}",
                    session_dir.display()
                );
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && restored.is_empty() => {
                return Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&session_dir)?;
            }
            Err(error) => return Err(error.into()),
        }
        crate::sys::set_secure_dir_permissions(&session_dir)?;
        let canonical_dir = session_dir.canonicalize()?;
        let managed_target = |source_key: &str| -> Option<PathBuf> {
            let source = Path::new(source_key);
            let file_name = source.file_name()?;
            let parent = source.parent()?.canonicalize().ok()?;
            (parent == canonical_dir).then(|| canonical_dir.join(file_name))
        };

        let keep = self
            .conv_db
            .load_artifact_assets(session_id)?
            .into_iter()
            .filter_map(|asset| managed_target(&asset.source_key))
            .collect::<HashSet<_>>();
        for artifact in restored {
            let Some(target) = managed_target(&artifact.asset.source_key) else {
                continue;
            };
            let mut temp = tempfile::NamedTempFile::new_in(&canonical_dir)?;
            crate::sys::set_secure_file_permissions(temp.path())?;
            temp.write_all(&artifact.bytes)?;
            temp.as_file_mut().sync_all()?;
            temp.persist(&target)
                .map_err(|error| error.error)
                .with_context(|| format!("restoring Artifact file: {}", target.display()))?;
        }
        for entry in std::fs::read_dir(&canonical_dir)? {
            let entry = entry?;
            let path = entry.path();
            if keep.contains(&path) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || metadata.is_file() {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub fn undo_last_turn(&self) -> Result<(usize, Option<String>)> {
        self.conv_db.undo_last_turn(&self.session())
    }

    pub fn redo_candidate(&self) -> Result<Option<RedoCandidate>> {
        self.conv_db.redo_candidate(&self.session())
    }

    pub fn load_redo_batch_prompts(
        &self,
        turn_id: &str,
        prompt_ids: &[String],
    ) -> Result<Vec<QueuedPrompt>> {
        self.conv_db
            .load_redo_batch_prompts(&self.session(), turn_id, prompt_ids)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_redo(
        &self,
        turn_id: &str,
        input_id: &str,
        input_kind: RedoInputKind,
        expected_revision: i64,
        content: &str,
        display_content: &str,
        owner_pid: u32,
    ) -> Result<RedoStart> {
        self.conv_db.begin_redo(
            &self.session(),
            turn_id,
            input_id,
            input_kind,
            expected_revision,
            content,
            display_content,
            owner_pid,
            &self.queue_session_id,
        )
    }

    pub fn add_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_usage(&self.usage_file(), usage)
    }

    pub fn add_auxiliary_usage(&self, usage: &Usage) -> Result<()> {
        self.init_files()?;
        usage::add_auxiliary_usage(&self.usage_file(), usage)
    }

    #[allow(dead_code)]
    pub fn usage_snapshot(&self) -> Result<UsageSnapshot> {
        usage::snapshot(&self.usage_file())
    }

    /// Lifetime token total of the current session (survives compaction,
    /// zeroed by /reset). This is the Σ shown in the REPL/WebUI footer.
    pub fn session_cumulative_tokens(&self) -> Result<u64> {
        self.conv_db.session_token_total(&self.session())
    }

    /// Same Σ, plus the prompt and cache-read halves the cumulative cache rate
    /// is computed from.
    pub fn session_cumulative_token_totals(&self) -> Result<TurnTokens> {
        self.conv_db.session_token_totals(&self.session())
    }

    pub fn clear_last_usage(&self) -> Result<()> {
        usage::clear_last_usage(&self.usage_file())
    }

    #[allow(dead_code)]
    pub fn has_running_turns(&self) -> Result<bool> {
        self.conv_db.has_running_turns(&self.session())
    }

    #[allow(dead_code)]
    pub fn running_turn_summaries(&self) -> Result<Vec<String>> {
        self.conv_db.running_turn_summaries(&self.session())
    }

    #[allow(dead_code)]
    pub fn running_turn_summaries_excluding(&self, exclude_turn_id: &str) -> Result<Vec<String>> {
        self.conv_db
            .running_turn_summaries_excluding(&self.session(), exclude_turn_id)
    }

    #[allow(dead_code)]
    pub fn migrate_from_jsonl(&self) -> Result<usize> {
        let jsonl_path = self.conversation_file();
        self.conv_db
            .migrate_from_jsonl(&self.session(), &jsonl_path)
    }

    fn conversation_file(&self) -> PathBuf {
        self.state_dir.join("conversation.jsonl")
    }

    fn usage_file(&self) -> PathBuf {
        self.state_dir.join("usage.json")
    }

    fn profile_file(&self) -> PathBuf {
        self.state_dir.join("profile.md")
    }

    fn prompt_fingerprint_file(&self) -> PathBuf {
        let key = blake3::hash(self.session().as_bytes()).to_hex();
        self.state_dir
            .join("prompt-fingerprints")
            .join(format!("{key}.sha256"))
    }
}

fn artifact_media_type(path: &Path) -> (&'static str, &'static str) {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "md" | "markdown" => ("text/markdown; charset=utf-8", "markdown"),
        "html" | "htm" => ("text/html; charset=utf-8", "html"),
        "pdf" => ("application/pdf", "pdf"),
        "json" | "jsonl" => ("application/json; charset=utf-8", "json"),
        "txt" | "log" | "csv" | "tsv" => ("text/plain; charset=utf-8", "text"),
        "css" => ("text/css; charset=utf-8", "code"),
        "js" | "mjs" | "cjs" => ("text/javascript; charset=utf-8", "code"),
        "xml" => ("application/xml; charset=utf-8", "code"),
        "rs" | "jsx" | "ts" | "tsx" | "py" | "go" | "java" | "c" | "cc" | "cpp" | "h" | "hpp"
        | "cs" | "rb" | "php" | "swift" | "kt" | "kts" | "sh" | "bash" | "zsh" | "fish"
        | "toml" | "yaml" | "yml" | "scss" | "sql" => ("text/plain; charset=utf-8", "code"),
        _ => ("application/octet-stream", "file"),
    }
}

fn prompt_fingerprint(system_prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(system_prompt.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[allow(dead_code)]
fn turn_chars(turn: &Turn) -> usize {
    turn.user_content.chars().count()
        + turn.assistant_content.chars().count()
        + turn
            .assistant_reasoning
            .as_deref()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
        + turn
            .tool_reports
            .iter()
            .map(|r| r.chars().count())
            .sum::<usize>()
        + turn
            .question_exchanges
            .iter()
            .filter_map(|exchange| serde_json::to_string(exchange).ok())
            .map(|exchange| exchange.chars().count())
            .sum::<usize>()
        + turn
            .followups
            .iter()
            .map(|followup| {
                followup.content.chars().count()
                    + followup
                        .preceding_assistant_content
                        .as_deref()
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or(0)
                    + followup
                        .preceding_assistant_reasoning
                        .as_deref()
                        .map(str::chars)
                        .map(Iterator::count)
                        .unwrap_or(0)
            })
            .sum::<usize>()
}

fn turns_to_entries(turns: Vec<Turn>) -> Vec<StoredConversationEntry> {
    let mut entries = Vec::with_capacity(turns.len() * 3);
    for turn in turns {
        let ts = turn.assistant_timestamp.clone().unwrap_or_default();
        entries.push(StoredConversationEntry {
            timestamp: turn.user_timestamp,
            role: "user".to_string(),
            content: turn.user_content,
            reasoning: None,
        });
        for exchange in &turn.question_exchanges {
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "assistant_clarification".to_string(),
                content: crate::question::assistant_exchange_text(exchange),
                reasoning: None,
            });
            entries.push(StoredConversationEntry {
                timestamp: exchange.answered_at.clone(),
                role: "user_clarification".to_string(),
                content: crate::question::user_exchange_text(exchange),
                reasoning: None,
            });
        }
        for followup in turn.followups {
            if followup
                .preceding_assistant_content
                .as_deref()
                .is_some_and(|content| !content.trim().is_empty())
                || followup
                    .preceding_assistant_reasoning
                    .as_deref()
                    .is_some_and(|reasoning| !reasoning.trim().is_empty())
            {
                entries.push(StoredConversationEntry {
                    timestamp: followup.submitted_at.clone(),
                    role: "assistant".to_string(),
                    content: followup.preceding_assistant_content.unwrap_or_default(),
                    reasoning: followup.preceding_assistant_reasoning,
                });
            }
            entries.push(StoredConversationEntry {
                timestamp: followup.submitted_at,
                role: "user".to_string(),
                content: followup.content,
                reasoning: None,
            });
        }
        entries.push(StoredConversationEntry {
            timestamp: ts.clone(),
            role: "assistant".to_string(),
            content: turn.assistant_content,
            reasoning: turn.assistant_reasoning,
        });
        for report in turn.tool_reports {
            entries.push(StoredConversationEntry {
                timestamp: ts.clone(),
                role: "assistant".to_string(),
                content: report,
                reasoning: None,
            });
        }
    }
    entries
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredConversationEntry {
    pub timestamp: String,
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub reasoning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();

        store.init_files().unwrap();
        assert!(!temp.path().join("state/miyu.log").exists());

        store.start_turn("turn_1", "hello", 999999).unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, TurnStatus::Running);
        assert_eq!(turns[0].assistant_content, pending_placeholder());

        store.complete_turn("turn_1", "hi there", None).unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Completed);
        assert_eq!(turns[0].assistant_content, "hi there");
    }

    #[test]
    fn question_exchange_persists_with_user_role_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();
        store.start_turn("turn_1", "配置它", 999999).unwrap();
        let request = crate::question::QuestionRequest {
            questions: vec![crate::question::QuestionPrompt {
                header: "范围".to_string(),
                question: "修改哪些部分？".to_string(),
                options: vec![crate::question::QuestionOption {
                    label: "全部".to_string(),
                    description: String::new(),
                }],
                multiple: false,
                custom: true,
            }],
        };
        let exchange =
            crate::question::QuestionExchange::new(request, vec![vec!["全部".to_string()]])
                .unwrap();
        store.append_question_exchange("turn_1", &exchange).unwrap();
        store.complete_turn("turn_1", "已经配置。", None).unwrap();

        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].question_exchanges, vec![exchange]);
        let history = store.load_conversation().unwrap();
        assert_eq!(history[1].role, "assistant_clarification");
        assert_eq!(history[2].role, "user_clarification");
        assert!(history[2].content.contains("全部"));
    }

    #[test]
    fn interrupt_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();

        store.start_turn("turn_1", "do something", 999999).unwrap();
        store.interrupt_turn("turn_1").unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Interrupted);
        assert_eq!(turns[0].assistant_content, interrupted_text());
    }

    #[test]
    fn interrupted_turn_materializes_persisted_journal_output() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();
        store
            .start_turn("turn_journal", "long task", 999999)
            .unwrap();
        store
            .append_turn_journal_event(
                "turn_journal",
                0,
                0,
                "assistant_content",
                None,
                None,
                Some("first persisted part"),
                None,
                None,
            )
            .unwrap();
        store
            .append_turn_journal_event(
                "turn_journal",
                0,
                0,
                "assistant_reasoning",
                None,
                None,
                Some("private reasoning"),
                None,
                None,
            )
            .unwrap();
        store.interrupt_turn("turn_journal").unwrap();

        let turn = store.load_turns().unwrap().remove(0);
        assert_eq!(turn.status, TurnStatus::Interrupted);
        assert!(turn.assistant_content.contains("first persisted part"));
        assert!(turn.assistant_content.contains(interrupted_text()));
        assert_eq!(
            turn.assistant_reasoning.as_deref(),
            Some("private reasoning")
        );
        assert_eq!(turn.journal_events.len(), 2);
    }

    #[test]
    fn superseded_journal_keeps_completed_tool_events_without_partial_text() {
        let (_temp, store) = test_store();
        store.start_turn("superseded", "long task", 999999).unwrap();
        store
            .append_turn_journal_event(
                "superseded",
                0,
                0,
                "assistant_content",
                None,
                None,
                Some("discarded partial answer"),
                None,
                None,
            )
            .unwrap();
        store
            .append_turn_journal_event(
                "superseded",
                0,
                0,
                "tool_call",
                Some("call-1"),
                Some("read_file"),
                Some("{\"path\":\"README.md\"}"),
                None,
                None,
            )
            .unwrap();
        store
            .append_turn_journal_event(
                "superseded",
                0,
                0,
                "tool_result",
                Some("call-1"),
                Some("read_file"),
                Some("completed tool output"),
                None,
                Some(true),
            )
            .unwrap();
        store
            .supersede_turn_journal_segment("superseded", 0, 0)
            .unwrap();

        let turn = store.load_turns().unwrap().remove(0);
        assert!(!turn
            .journal_events
            .iter()
            .any(|event| event.kind == "assistant_content"));
        assert!(turn
            .journal_events
            .iter()
            .any(|event| event.kind == "tool_call"));
        assert!(turn
            .journal_events
            .iter()
            .any(|event| event.kind == "tool_result"));
    }

    #[test]
    fn recover_stale_running() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();

        store.start_turn("turn_1", "task a", 999999).unwrap();
        store.start_turn("turn_2", "task b", 999999).unwrap();
        assert!(store.has_running_turns().unwrap());

        let recovered = store.recover_stale_turns().unwrap();
        assert_eq!(recovered, 2);

        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|t| t.status == TurnStatus::Interrupted));
    }

    #[test]
    fn recover_stale_skips_alive_owner() {
        let (_temp, store) = test_store();

        let current_pid = std::process::id();
        store
            .start_turn("turn_1", "终端1的prompt", current_pid)
            .unwrap();
        store.start_turn("turn_dead", "孤儿turn", 999999).unwrap();

        let recovered = store.recover_stale_turns().unwrap();
        assert_eq!(recovered, 1);

        let turns = store.load_turns().unwrap();
        let turn1 = turns.iter().find(|t| t.turn_id == "turn_1").unwrap();
        assert_eq!(turn1.status, TurnStatus::Running);
        assert_eq!(turn1.assistant_content, pending_placeholder());

        let dead = turns.iter().find(|t| t.turn_id == "turn_dead").unwrap();
        assert_eq!(dead.status, TurnStatus::Interrupted);
    }

    #[test]
    fn interrupt_keeps_consumed_prompts_attached_to_the_interrupted_turn() {
        let (_temp, store) = test_store();
        store
            .enqueue_prompt("q1", "followup", "followup", &[])
            .unwrap();
        store.start_turn("turn_1", "initial", 999999).unwrap();
        store
            .consume_queued_prompts(
                "turn_1",
                &[("q1".to_string(), "followup".to_string())],
                None,
                None,
            )
            .unwrap();

        store.interrupt_turn("turn_1").unwrap();

        assert!(store.load_queued_prompts().unwrap().is_empty());
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Interrupted);
        assert_eq!(turns[0].followups.len(), 1);
        assert_eq!(turns[0].followups[0].prompt_id, "q1");
    }

    #[test]
    fn stale_turn_recovery_keeps_consumed_prompts_consumed() {
        let (_temp, store) = test_store();
        store
            .enqueue_prompt("q1", "followup", "followup", &[])
            .unwrap();
        store.start_turn("turn_1", "initial", 999999).unwrap();
        store
            .consume_queued_prompts(
                "turn_1",
                &[("q1".to_string(), "followup".to_string())],
                None,
                None,
            )
            .unwrap();

        assert_eq!(store.recover_stale_turns().unwrap(), 1);
        assert!(store.load_queued_prompts().unwrap().is_empty());
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].status, TurnStatus::Interrupted);
        assert_eq!(turns[0].followups[0].prompt_id, "q1");
    }

    #[test]
    fn stale_turn_recovery_consumes_accepted_queued_prompts() {
        let (_temp, store) = test_store();
        store.start_turn("turn_1", "initial", 999999).unwrap();
        store
            .append_turn_journal_event(
                "turn_1",
                0,
                0,
                "assistant_content",
                None,
                None,
                Some("partial answer"),
                None,
                None,
            )
            .unwrap();
        let target = store.running_turn_queue_target().unwrap().unwrap();
        store
            .enqueue_prompt_for_target(&target, "q1", "followup", "followup", &[])
            .unwrap();

        assert_eq!(store.recover_stale_turns().unwrap(), 1);
        assert!(store
            .load_queued_prompts_for_target(&target)
            .unwrap()
            .is_empty());
        let turn = store.load_turns().unwrap().remove(0);
        assert_eq!(turn.status, TurnStatus::Interrupted);
        assert_eq!(turn.followups.len(), 1);
        assert_eq!(turn.followups[0].prompt_id, "q1");
        assert_eq!(
            turn.followups[0].preceding_assistant_content.as_deref(),
            Some("partial answer")
        );
        assert!(turn
            .journal_events
            .iter()
            .any(|event| event.kind == "queued_prompts_consumed"));
    }

    #[test]
    fn finished_turn_cleanup_preserves_a_late_queued_prompt() {
        let (_temp, store) = test_store();
        store
            .start_turn("turn_1", "initial", std::process::id())
            .unwrap();
        store.complete_turn("turn_1", "answer", None).unwrap();
        store
            .enqueue_prompt("late", "followup", "followup", &[])
            .unwrap();

        assert_eq!(store.discard_queued_prompts().unwrap(), 1);
        let turn = store.load_turns().unwrap().remove(0);
        assert_eq!(turn.followups.len(), 1);
        assert_eq!(turn.followups[0].prompt_id, "late");
        assert_eq!(
            turn.followups[0].preceding_assistant_content.as_deref(),
            Some("answer")
        );
    }

    #[test]
    fn cancelled_turn_cleanup_deletes_queued_prompts_without_folding() {
        let (_temp, store) = test_store();
        store
            .start_turn("turn_1", "initial", std::process::id())
            .unwrap();
        store
            .enqueue_prompt("q1", "排队消息", "排队消息", &[])
            .unwrap();
        store.interrupt_turn("turn_1").unwrap();

        let dropped = store.delete_queued_prompts().unwrap();
        assert_eq!(dropped, vec!["q1".to_string()]);
        // Neither still queued nor folded into the turn as a follow-up.
        assert!(store.load_queued_prompts().unwrap().is_empty());
        let turn = store.load_turns().unwrap().remove(0);
        assert!(turn.followups.is_empty());
        // Idempotent on an already-empty queue.
        assert!(store.delete_queued_prompts().unwrap().is_empty());
    }

    #[test]
    fn undo_removes_last_turn() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        })
        .unwrap();

        store.start_turn("turn_1", "hello", 999999).unwrap();
        store.complete_turn("turn_1", "hi", None).unwrap();
        store.start_turn("turn_2", "bye", 999999).unwrap();
        store.complete_turn("turn_2", "goodbye", None).unwrap();

        let (removed, prompt) = store.undo_last_turn().unwrap();
        assert_eq!(removed, 1);
        assert_eq!(prompt.as_deref(), Some("bye"));

        let turns = store.load_turns().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn_id, "turn_1");
    }

    fn test_paths(root: &Path) -> MiyuPaths {
        MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish/miyu.fish"),
            bash_hook_file: root.join("shell/bash-hook.sh"),
            zsh_hook_file: root.join("shell/zsh-hook.zsh"),
            scripts_dir: root.join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        }
    }

    fn test_store() -> (tempfile::TempDir, StateStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        (temp, store)
    }

    #[test]
    fn platform_access_grants_are_cached_persisted_and_audited() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let store = StateStore::new(&paths).unwrap();
        let peer = StateStore::new(&paths).unwrap();
        let key = PlatformAccessGrantKey {
            platform: "onebot".to_string(),
            account_scope: GLOBAL_PLATFORM_ACCOUNT_SCOPE.to_string(),
            permission: "private_whitelist".to_string(),
            subject_kind: "user".to_string(),
            subject_id: "2477342916".to_string(),
        };
        let actor = PlatformAccessActor {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            user_id: "42".to_string(),
            conversation_kind: "private".to_string(),
            conversation_id: "42".to_string(),
            message_id: "message-1".to_string(),
        };

        assert!(store.add_platform_access_grant(&key, &actor).unwrap());
        assert!(!store.add_platform_access_grant(&key, &actor).unwrap());
        assert!(store.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert!(peer.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert!(store.has_platform_access_grant(
            "onebot",
            "another-bot",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert_eq!(store.platform_access_grants("onebot").unwrap().len(), 1);

        let reopened = StateStore::new(&paths).unwrap();
        assert!(reopened.has_platform_access_grant(
            "onebot",
            "20000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert!(reopened.remove_platform_access_grant(&key, &actor).unwrap());
        assert!(!reopened.remove_platform_access_grant(&key, &actor).unwrap());
        assert!(!reopened.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert!(!store.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));
        assert!(!peer.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "2477342916"
        ));

        let denied_key = PlatformAccessGrantKey {
            subject_id: "99".to_string(),
            ..key.clone()
        };
        let denied = store
            .mutate_platform_access_grant_if_authorized(
                &denied_key,
                &actor,
                PlatformAccessMutation::Grant,
                &PlatformAccessAuthorization {
                    statically_authorized: false,
                    dynamic_key: PlatformAccessGrantKey {
                        platform: "onebot".to_string(),
                        account_scope: "10000".to_string(),
                        permission: "administrator".to_string(),
                        subject_kind: "user".to_string(),
                        subject_id: "42".to_string(),
                    },
                },
            )
            .unwrap();
        assert_eq!(denied, PlatformAccessMutationResult::Unauthorized);
        assert!(!store.has_platform_access_grant(
            "onebot",
            "10000",
            "private_whitelist",
            "user",
            "99"
        ));

        let conn = rusqlite::Connection::open(paths.state_dir.join("conversation.db")).unwrap();
        let audit_count: i64 = conn
            .query_row("SELECT count(*) FROM platform_access_audit", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(audit_count, 2);
    }

    #[test]
    fn user_attachment_moves_from_staged_to_turn_and_cascades() {
        let (_temp, store) = test_store();
        let attachment = UserAttachment {
            attachment_id: "att_test".to_string(),
            file_name: "notes.md".to_string(),
            mime: "text/markdown".to_string(),
            kind: "text".to_string(),
            size_bytes: 7,
            width: 0,
            height: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        store.save_user_attachment(&attachment, b"content").unwrap();
        assert_eq!(
            store
                .load_staged_user_attachments(&[attachment.attachment_id.clone()])
                .unwrap()[0]
                .bytes,
            b"content"
        );

        store
            .reserve_user_attachments(&[attachment.attachment_id.clone()], "run_test")
            .unwrap();
        store
            .start_turn_with_display(
                "turn_test",
                "visible\n\n<user-attachment>content</user-attachment>",
                "visible",
                std::process::id(),
                Some("run_test"),
            )
            .unwrap();
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].display_content, "visible");
        assert_eq!(turns[0].attachments, vec![attachment.clone()]);
        assert!(store
            .load_staged_user_attachments(&[attachment.attachment_id.clone()])
            .is_err());

        store.reset_conversation().unwrap();
        assert!(store
            .load_user_attachment_by_id(&attachment.attachment_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn session_crud_switching_and_persona_adoption() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        // Migrated/default rows start persona-less and are claimed on adoption.
        store.adopt_sessions_for_persona("miyu").unwrap();
        let default_id = store.session_id();
        let default = store.session_record(&default_id).unwrap().unwrap();
        assert_eq!(default.persona, "miyu");

        store.start_turn("t1", "hello", std::process::id()).unwrap();
        store.complete_turn("t1", "hi", None).unwrap();

        let created = store
            .create_session("miyu", "旅行计划", "user", None)
            .unwrap();
        store.switch_session(&created.session_id).unwrap();
        assert_eq!(&*store.session_id(), created.session_id.as_str());
        // The new session starts empty; history stays in the old session.
        assert!(store.load_visible_turns().unwrap().is_empty());

        // The pointer is persisted: an independent store resolves to it.
        let reopened = StateStore::new(&test_paths(temp.path())).unwrap();
        assert_eq!(&*reopened.session_id(), created.session_id.as_str());

        let listed = store.list_sessions("miyu", false).unwrap();
        assert_eq!(listed.len(), 2);
        let default_overview = listed
            .iter()
            .find(|overview| overview.record.session_id == &*default_id)
            .unwrap();
        assert_eq!(default_overview.turn_count, 1);
        assert_eq!(default_overview.last_user_content.as_deref(), Some("hello"));

        assert!(store
            .find_session_by_name("miyu", "旅行计划")
            .unwrap()
            .is_some());
        store.rename_session(&created.session_id, "新名字").unwrap();
        assert!(store
            .find_session_by_name("miyu", "旅行计划")
            .unwrap()
            .is_none());

        store
            .set_session_archived(&created.session_id, true)
            .unwrap();
        assert_eq!(store.list_sessions("miyu", false).unwrap().len(), 1);
        assert_eq!(store.list_sessions("miyu", true).unwrap().len(), 2);

        // Deleting a session cascades its turns away.
        store.delete_session(&default_id).unwrap();
        assert!(store.session_record(&default_id).unwrap().is_none());
        assert_eq!(store.list_sessions("miyu", true).unwrap().len(), 1);

        // A dangling pointer self-heals back to a default session.
        store.delete_session(&created.session_id).unwrap();
        let healed = StateStore::new(&test_paths(temp.path())).unwrap();
        assert!(healed
            .session_record(&healed.session_id())
            .unwrap()
            .is_some());
    }

    #[test]
    fn persona_reset_clears_active_local_and_onebot_contexts_only() {
        let (_temp, store) = test_store();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let current = store.session_id().to_string();
        let local = store.create_session("miyu", "local", "user", None).unwrap();
        let archived = store
            .create_session("miyu", "archived", "user", None)
            .unwrap();
        store
            .set_session_archived(&archived.session_id, true)
            .unwrap();
        let other_persona = store
            .create_session("other", "other", "user", None)
            .unwrap();
        let qq = store.create_session("miyu", "qq", "user", None).unwrap();
        store.set_session_archived(&qq.session_id, true).unwrap();
        store
            .bind_platform_session(
                &PlatformSessionBindingKey {
                    platform: "onebot".to_string(),
                    account_id: "10000".to_string(),
                    conversation_kind: "group".to_string(),
                    conversation_id: "42".to_string(),
                    participant_id: None,
                    persona: "miyu".to_string(),
                },
                &qq.session_id,
            )
            .unwrap();
        let subagent = store
            .create_session("miyu", "child", "subagent", Some(&local.session_id))
            .unwrap();
        let archived_child = store
            .create_session(
                "miyu",
                "archived-child",
                "subagent",
                Some(&archived.session_id),
            )
            .unwrap();

        let sessions = [
            current.clone(),
            local.session_id.clone(),
            archived.session_id.clone(),
            other_persona.session_id.clone(),
            qq.session_id.clone(),
            subagent.session_id.clone(),
            archived_child.session_id.clone(),
        ];
        for (index, session_id) in sessions.iter().enumerate() {
            let pinned = store.pinned(session_id);
            let turn_id = format!("reset-scope-{index}");
            pinned
                .start_turn(&turn_id, "before", std::process::id())
                .unwrap();
            pinned.complete_turn(&turn_id, "after", None).unwrap();
        }

        let targets = store.persona_reset_session_ids("miyu", "onebot").unwrap();
        assert!(targets.contains(&current));
        assert!(targets.contains(&local.session_id));
        assert!(targets.contains(&qq.session_id));
        assert!(targets.contains(&subagent.session_id));
        assert!(!targets.contains(&archived.session_id));
        assert!(!targets.contains(&archived_child.session_id));
        assert!(!targets.contains(&other_persona.session_id));

        let cleared = store.reset_persona_contexts("miyu", "onebot").unwrap();
        assert_eq!(cleared, targets);
        for session_id in [
            &current,
            &local.session_id,
            &qq.session_id,
            &subagent.session_id,
        ] {
            assert!(store.pinned(session_id).load_turns().unwrap().is_empty());
        }
        for session_id in [
            &archived.session_id,
            &archived_child.session_id,
            &other_persona.session_id,
        ] {
            assert_eq!(store.pinned(session_id).load_turns().unwrap().len(), 1);
        }
        assert_eq!(
            store.platform_session_bindings("miyu", "onebot").unwrap()[0].session_id,
            qq.session_id
        );
    }

    fn platform_binding_key(
        conversation_id: &str,
        participant_id: Option<&str>,
        persona: &str,
    ) -> PlatformSessionBindingKey {
        PlatformSessionBindingKey {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            conversation_kind: "group".to_string(),
            conversation_id: conversation_id.to_string(),
            participant_id: participant_id.map(str::to_string),
            persona: persona.to_string(),
        }
    }

    fn plugin_scope(conversation_id: &str) -> PlatformPluginScopeKey {
        PlatformPluginScopeKey {
            plugin_id: "reply_processor".to_string(),
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            conversation_kind: "group".to_string(),
            conversation_id: conversation_id.to_string(),
        }
    }

    #[test]
    fn platform_bindings_survive_rename_and_isolate_personas() {
        let (_temp, store) = test_store();
        let miyu_session = store
            .create_session("miyu", "old display name", "user", None)
            .unwrap();
        let other_session = store
            .create_session("other", "another display name", "user", None)
            .unwrap();
        let miyu_key = platform_binding_key("20000", None, "miyu");
        let other_key = platform_binding_key("20000", None, "other");

        store
            .bind_platform_session(&miyu_key, &miyu_session.session_id)
            .unwrap();
        store
            .bind_platform_session(&other_key, &other_session.session_id)
            .unwrap();
        store
            .rename_session(&miyu_session.session_id, "new display name")
            .unwrap();

        assert_eq!(
            store.find_platform_session_binding(&miyu_key).unwrap(),
            Some(miyu_session.session_id.clone())
        );
        // `None` and an empty participant are the same database identity.
        let empty_participant_key = platform_binding_key("20000", Some(""), "miyu");
        assert_eq!(
            store
                .find_platform_session_binding(&empty_participant_key)
                .unwrap(),
            Some(miyu_session.session_id.clone())
        );
        assert_eq!(
            store.find_platform_session_binding(&other_key).unwrap(),
            Some(other_session.session_id)
        );

        store.delete_session(&miyu_session.session_id).unwrap();
        assert_eq!(
            store.find_platform_session_binding(&miyu_key).unwrap(),
            None
        );
    }

    #[test]
    fn persona_scope_rename_migrates_sessions_bindings_and_affection() {
        let (_temp, store) = test_store();
        let session = store
            .create_session("old", "QQ group", "user", None)
            .unwrap();
        let old_binding = platform_binding_key("20000", None, "old");
        store
            .bind_platform_session(&old_binding, &session.session_id)
            .unwrap();
        store
            .set_persona_current_session("old", &session.session_id)
            .unwrap();
        let scope = PlatformPluginScopeKey {
            plugin_id: "real_context".to_string(),
            ..plugin_scope("20000")
        };
        store
            .plugin_put_json(
                &scope,
                "affection_profile:old",
                &serde_json::json!({"score": 42}),
            )
            .unwrap();

        store.rename_persona_scope("old", "new").unwrap();

        assert_eq!(
            store
                .session_record(&session.session_id)
                .unwrap()
                .unwrap()
                .persona,
            "new"
        );
        assert!(store
            .find_platform_session_binding(&old_binding)
            .unwrap()
            .is_none());
        let new_binding = platform_binding_key("20000", None, "new");
        assert_eq!(
            store.find_platform_session_binding(&new_binding).unwrap(),
            Some(session.session_id.clone())
        );
        assert_eq!(
            store.persona_current_session("new").unwrap(),
            Some(session.session_id)
        );
        assert!(store
            .plugin_get_json::<serde_json::Value>(&scope, "affection_profile:old")
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .plugin_get_json::<serde_json::Value>(&scope, "affection_profile:new")
                .unwrap()
                .unwrap()["score"],
            42
        );
    }

    #[test]
    fn local_session_listing_excludes_platform_owned_history() {
        let (_temp, store) = test_store();
        let local = store
            .create_session("miyu", "shared name", "user", None)
            .unwrap();
        let platform = store
            .create_session("miyu", "shared name", "user", None)
            .unwrap();
        let key = platform_binding_key("20000", None, "miyu");
        store
            .bind_platform_session(&key, &platform.session_id)
            .unwrap();

        let all_ids = store
            .list_sessions("miyu", false)
            .unwrap()
            .into_iter()
            .map(|overview| overview.record.session_id)
            .collect::<Vec<_>>();
        assert!(all_ids.contains(&local.session_id));
        assert!(all_ids.contains(&platform.session_id));

        let local_ids = store
            .list_local_sessions("miyu", false)
            .unwrap()
            .into_iter()
            .map(|overview| overview.record.session_id)
            .collect::<Vec<_>>();
        assert!(local_ids.contains(&local.session_id));
        assert!(!local_ids.contains(&platform.session_id));
        assert!(!store.is_platform_session(&local.session_id).unwrap());
        assert!(store.is_platform_session(&platform.session_id).unwrap());
        assert_eq!(
            store
                .find_local_session_by_name("miyu", "SHARED NAME")
                .unwrap()
                .unwrap()
                .session_id,
            local.session_id
        );
    }

    #[test]
    fn platform_binding_overwrite_and_conflict_are_atomic() {
        let (_temp, store) = test_store();
        let session_a = store.create_session("miyu", "a", "user", None).unwrap();
        let session_b = store.create_session("miyu", "b", "user", None).unwrap();
        let session_c = store.create_session("miyu", "c", "user", None).unwrap();
        let key_a = platform_binding_key("group-a", None, "miyu");
        let key_b = platform_binding_key("group-b", None, "miyu");

        store
            .bind_platform_session(&key_a, &session_a.session_id)
            .unwrap();
        store
            .bind_platform_session(&key_b, &session_b.session_id)
            .unwrap();

        let error = store
            .bind_platform_session(&key_a, &session_b.session_id)
            .unwrap_err();
        assert!(error.to_string().contains("already bound"));
        assert_eq!(
            store.find_platform_session_binding(&key_a).unwrap(),
            Some(session_a.session_id)
        );
        assert_eq!(
            store.find_platform_session_binding(&key_b).unwrap(),
            Some(session_b.session_id)
        );

        store
            .bind_platform_session(&key_a, &session_c.session_id)
            .unwrap();
        assert_eq!(
            store.find_platform_session_binding(&key_a).unwrap(),
            Some(session_c.session_id)
        );
        assert!(store.unbind_platform_session(&key_a).unwrap());
        assert!(!store.unbind_platform_session(&key_a).unwrap());
    }

    #[test]
    fn concurrent_platform_bind_rejects_session_sharing() {
        let (temp, store) = test_store();
        let second_store = StateStore::new(&test_paths(temp.path())).unwrap();
        let session = store
            .create_session("miyu", "shared target", "user", None)
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = [store.clone(), second_store]
            .into_iter()
            .zip(["group-a", "group-b"])
            .map(|(store, conversation_id)| {
                let barrier = barrier.clone();
                let session_id = session.session_id.clone();
                let key = platform_binding_key(conversation_id, None, "miyu");
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = store.bind_platform_session(&key, &session_id);
                    (key, result)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_err()).count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|(key, _)| store.find_platform_session_binding(key).unwrap().is_some())
                .count(),
            1
        );
    }

    #[test]
    fn concurrent_platform_claim_converges_on_one_session() {
        let (temp, store) = test_store();
        let second_store = StateStore::new(&test_paths(temp.path())).unwrap();
        let session_a = store.create_session("miyu", "a", "user", None).unwrap();
        let session_b = store.create_session("miyu", "b", "user", None).unwrap();
        let key = platform_binding_key("same-group", None, "miyu");
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = [
            (store.clone(), session_a.session_id.clone()),
            (second_store, session_b.session_id.clone()),
        ]
        .into_iter()
        .map(|(store, candidate)| {
            let barrier = barrier.clone();
            let key = key.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.claim_platform_session(&key, &candidate).unwrap()
            })
        })
        .collect::<Vec<_>>();
        barrier.wait();
        let winners = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(winners[0], winners[1]);
        assert_eq!(
            store.find_platform_session_binding(&key).unwrap(),
            Some(winners[0].clone())
        );
        assert!(winners[0] == session_a.session_id || winners[0] == session_b.session_id);
    }

    #[test]
    fn platform_session_creation_is_bound_atomically() {
        let (_temp, store) = test_store();
        let key = platform_binding_key("atomic-group", None, "miyu");
        let (platform, created) = store
            .create_or_get_platform_session(&key, "platform")
            .unwrap();
        assert!(created);
        assert_eq!(
            store.find_platform_session_binding(&key).unwrap(),
            Some(platform.session_id.clone())
        );
        assert!(!store
            .list_local_sessions("miyu", false)
            .unwrap()
            .iter()
            .any(|entry| entry.record.session_id == platform.session_id));

        let (same, created) = store
            .create_or_get_platform_session(&key, "ignored")
            .unwrap();
        assert!(!created);
        assert_eq!(same.session_id, platform.session_id);
    }

    #[test]
    fn platform_plugin_json_is_shared_across_personas_and_supports_deletion() {
        let (_temp, store) = test_store();
        let scope = plugin_scope("20000");
        let value = vec!["image-a".to_string(), "image-b".to_string()];
        store
            .plugin_put_json(&scope, "recent_images", &value)
            .unwrap();
        let replacement = vec!["image-c".to_string()];
        store
            .plugin_put_json(&scope, "recent_images", &replacement)
            .unwrap();

        // Pinned stores represent independent persona sessions but share the
        // external-conversation plugin scope.
        let miyu_session = store.create_session("miyu", "miyu", "user", None).unwrap();
        let other_session = store
            .create_session("other", "other", "user", None)
            .unwrap();
        let miyu_store = store.pinned(&miyu_session.session_id);
        let other_store = store.pinned(&other_session.session_id);
        let from_miyu: Option<Vec<String>> =
            miyu_store.plugin_get_json(&scope, "recent_images").unwrap();
        let from_other: Option<Vec<String>> = other_store
            .plugin_get_json(&scope, "recent_images")
            .unwrap();
        assert_eq!(from_miyu, Some(replacement.clone()));
        assert_eq!(from_other, Some(replacement));

        store.plugin_put_json(&scope, "mode", &"image").unwrap();
        assert!(store.plugin_delete_key(&scope, "recent_images").unwrap());
        let deleted: Option<Vec<String>> = store.plugin_get_json(&scope, "recent_images").unwrap();
        assert_eq!(deleted, None);
        assert_eq!(store.plugin_delete_scope(&scope).unwrap(), 1);
        assert!(!store.plugin_delete_key(&scope, "mode").unwrap());
    }

    #[test]
    fn concurrent_platform_plugin_updates_do_not_lose_values() {
        let (temp, first) = test_store();
        let second = StateStore::new(&test_paths(temp.path())).unwrap();
        let scope = plugin_scope("atomic-group");
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let handles = (0..8)
            .map(|value| {
                let store = if value % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let scope = scope.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .plugin_update_json(&scope, "values", |current: Option<Vec<usize>>| {
                            let mut values = current.unwrap_or_default();
                            values.push(value);
                            Ok(Some(values))
                        })
                        .unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        let mut values: Vec<usize> = first.plugin_get_json(&scope, "values").unwrap().unwrap();
        values.sort_unstable();
        assert_eq!(values, (0..8).collect::<Vec<_>>());
    }

    fn platform_meme_ref(
        conversation_id: &str,
        message_id: &str,
        library: &str,
        meme_id: &str,
        direction: &str,
        created_at: &str,
    ) -> PlatformMemeRefRecord {
        PlatformMemeRefRecord {
            platform: "onebot".to_string(),
            account_id: "10000".to_string(),
            conversation_kind: "group".to_string(),
            conversation_id: conversation_id.to_string(),
            message_id: message_id.to_string(),
            library: library.to_string(),
            meme_id: meme_id.to_string(),
            direction: direction.to_string(),
            created_at: created_at.to_string(),
        }
    }

    #[test]
    fn platform_meme_refs_are_ordered_isolated_upserted_and_cleaned_by_ref() {
        let (_temp, store) = test_store();
        let later = platform_meme_ref(
            "group-a",
            "message-1",
            "secondary",
            "meme-b",
            "outbound",
            "2026-01-02T00:00:00Z",
        );
        let earlier = platform_meme_ref(
            "group-a",
            "message-1",
            "default",
            "meme-a",
            "inbound",
            "2026-01-01T00:00:00Z",
        );
        let other_conversation = platform_meme_ref(
            "group-b",
            "message-1",
            "default",
            "meme-a",
            "inbound",
            "2026-01-03T00:00:00Z",
        );
        store.put_platform_meme_ref(&later).unwrap();
        store.put_platform_meme_ref(&earlier).unwrap();
        store.put_platform_meme_ref(&other_conversation).unwrap();

        assert_eq!(
            store
                .platform_meme_refs_for_message("onebot", "10000", "group", "group-a", "message-1")
                .unwrap(),
            vec![earlier.clone(), later]
        );

        let mut updated = earlier;
        updated.direction = "outbound".to_string();
        updated.created_at = "2026-01-04T00:00:00Z".to_string();
        store.put_platform_meme_ref(&updated).unwrap();
        let records = store
            .platform_meme_refs_for_message("onebot", "10000", "group", "group-a", "message-1")
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1], updated);

        assert_eq!(
            store.delete_platform_meme_ref("default", "meme-a").unwrap(),
            2
        );
        assert!(store
            .platform_meme_refs_for_message("onebot", "10000", "group", "group-b", "message-1")
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .platform_meme_refs_for_message("onebot", "10000", "group", "group-a", "message-1")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn platform_meme_ref_rejects_invalid_direction() {
        let (_temp, store) = test_store();
        let record = platform_meme_ref(
            "group-a",
            "message-1",
            "default",
            "meme-a",
            "sideways",
            "2026-01-01T00:00:00Z",
        );
        assert!(store.put_platform_meme_ref(&record).is_err());
        assert!(store
            .platform_meme_refs_for_message("onebot", "10000", "group", "group-a", "message-1")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn wiping_the_persona_takes_the_subagent_rows_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let parent = store.session_id();
        let audit = store
            .create_session("miyu", "深挖", "subagent", Some(&parent))
            .unwrap();
        store
            .record_subagent_usage(&audit.session_id, None, None, None, 400, 100, 500, 200)
            .unwrap();
        assert_eq!(store.session_cumulative_token_totals().unwrap().total, 500);

        // Subagent usage lives on the session row, not in `turns` — clearing
        // the turns alone left every Σ still carrying it.
        store.reset_persona_contexts("miyu", "onebot").unwrap();
        assert_eq!(
            store.session_cumulative_token_totals().unwrap(),
            TurnTokens::default()
        );
    }

    #[test]
    fn a_subagents_tokens_land_in_the_launching_sessions_total() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let parent = store.session_id();

        let turn_id = "turn_parent_1";
        store
            .start_turn(turn_id, "问题", std::process::id())
            .unwrap();
        store
            .complete_turn_with_usage_and_model(
                turn_id,
                "答案",
                None,
                None,
                None,
                TurnTokens {
                    total: 1_000,
                    prompt: 900,
                    cache_read: 300,
                },
                false,
            )
            .unwrap();
        assert_eq!(
            store.session_cumulative_token_totals().unwrap(),
            TurnTokens {
                total: 1_000,
                prompt: 900,
                cache_read: 300
            }
        );

        let audit = store
            .create_session("miyu", "深挖", "subagent", Some(&parent))
            .unwrap();
        store
            .record_subagent_usage(&audit.session_id, None, None, None, 400, 100, 500, 200)
            .unwrap();

        // A subagent bills to the session that launched it, cache hits and all
        // — otherwise the most expensive thing a turn can do is invisible.
        assert_eq!(
            store.session_cumulative_token_totals().unwrap(),
            TurnTokens {
                total: 1_500,
                prompt: 1_300,
                cache_read: 500
            }
        );

        // A reset that left the audit sessions behind would zero the history
        // and still report a running total.
        store.reset_conversation().unwrap();
        assert_eq!(
            store.session_cumulative_token_totals().unwrap(),
            TurnTokens::default()
        );
    }

    #[test]
    fn a_subagent_run_recorded_before_the_cache_column_stays_out_of_the_rate() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let parent = store.session_id();
        let audit = store
            .create_session("miyu", "升级前的一次", "subagent", Some(&parent))
            .unwrap();
        // Exactly what the v19 migration leaves behind: usage recorded, cache
        // unknown (NULL). Counting its prompt with no hits to match turned a
        // measured 24% into 1% on the real database.
        store
            .conv_db()
            .record_legacy_subagent_usage_for_test(&audit.session_id, 1_111_360, 1_222_121)
            .unwrap();
        let totals = store.session_cumulative_token_totals().unwrap();
        assert_eq!(totals.total, 1_222_121);
        assert_eq!(
            totals.prompt, 0,
            "unknown cache must not claim a denominator"
        );
        assert_eq!(totals.cache_read, 0);
    }

    #[test]
    fn an_estimated_subagent_run_never_reaches_the_cache_denominator() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let parent = store.session_id();
        let audit = store
            .create_session("miyu", "估算的一次", "subagent", Some(&parent))
            .unwrap();
        // The provider reported nothing, so only the char estimate is known:
        // it inflates the total but must not pretend to be measured prompt.
        store
            .record_subagent_usage(&audit.session_id, None, None, None, 0, 0, 9_000, 0)
            .unwrap();
        let totals = store.session_cumulative_token_totals().unwrap();
        assert_eq!(totals.total, 9_000);
        assert_eq!(totals.prompt, 0);
        assert_eq!(totals.cache_read, 0);
    }

    #[test]
    fn subagent_audit_sessions_are_hidden_and_expire() {
        let temp = tempfile::tempdir().unwrap();
        let store = StateStore::new(&test_paths(temp.path())).unwrap();
        store.adopt_sessions_for_persona("miyu").unwrap();
        let parent = store.session_id();
        let audit = store
            .create_session("miyu", "探索代码库", "subagent", Some(&parent))
            .unwrap();
        let pinned = store.pinned(&audit.session_id);
        pinned
            .start_turn("sat_1", "task prompt", std::process::id())
            .unwrap();
        pinned
            .complete_turn("sat_1", "{\"ok\":true}", None)
            .unwrap();
        store
            .record_subagent_usage(
                &audit.session_id,
                Some("opencode"),
                Some("big-pickle"),
                Some(168000),
                100,
                50,
                150,
                40,
            )
            .unwrap();

        // Hidden from the user-facing session list.
        assert!(store
            .list_sessions("miyu", true)
            .unwrap()
            .iter()
            .all(|overview| overview.record.session_id != audit.session_id));
        let record = store.session_record(&audit.session_id).unwrap().unwrap();
        assert_eq!(record.kind, "subagent");
        assert_eq!(record.parent_session_id.as_deref(), Some(&*parent));

        // Fresh audit survives cleanup; a backdated one is removed with its
        // turns (FK cascade).
        assert_eq!(store.delete_subagent_sessions_older_than(7).unwrap(), 0);
        store
            .conv_db()
            .record_subagent_usage(&audit.session_id, None, None, None, 0, 0, 0, 0)
            .unwrap();
        // Backdate updated_at directly.
        store.conv_db().touch_session(&audit.session_id).unwrap();
        let backdated = (chrono::Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        // No public API to backdate; use a raw update via the test-only conv_db handle.
        {
            use rusqlite::params;
            let db_path = temp.path().join("state").join("conversation.db");
            let conn = rusqlite::Connection::open(db_path).unwrap();
            conn.execute(
                "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
                params![backdated, audit.session_id],
            )
            .unwrap();
        }
        assert_eq!(store.delete_subagent_sessions_older_than(7).unwrap(), 1);
        assert!(store.session_record(&audit.session_id).unwrap().is_none());
    }

    #[test]
    fn finished_turns_keep_a_replayable_transcript() {
        let (_temp, store) = test_store();
        store.init_files().unwrap();
        store.start_turn("t1", "改一下 README", 999_999).unwrap();
        let db = store.conv_db();
        for (kind, call_id, name, payload, ok) in [
            ("assistant_content", None, None, Some("这就去改。"), None),
            (
                "tool_call",
                Some("c1"),
                Some("edit_string"),
                Some("{\"path\":\"README.md\"}"),
                None,
            ),
            (
                "tool_result",
                Some("c1"),
                None,
                Some("1 处替换"),
                Some(true),
            ),
            ("tool_progress", Some("c1"), None, Some("忽略我"), None),
            ("assistant_content", None, None, Some("改好了。"), None),
        ] {
            db.append_turn_journal_event("t1", 0, 0, kind, call_id, name, payload, None, ok)
                .unwrap();
        }
        store.complete_turn("t1", "改好了。", None).unwrap();

        let replays = store.session_replay(5).unwrap();
        assert_eq!(replays.len(), 1);
        let entries = &replays[0].entries;
        assert_eq!(replays[0].display_content, "改一下 README");
        // Prose and tool blocks keep their original interleaving, and the
        // live-only progress ticks are gone.
        assert_eq!(
            entries,
            &vec![
                ReplayEntry::Text {
                    text: "这就去改。".to_string()
                },
                ReplayEntry::ToolCall {
                    name: "edit_string".to_string(),
                    arguments: "{\"path\":\"README.md\"}".to_string(),
                },
                ReplayEntry::ToolResult {
                    name: "edit_string".to_string(),
                    ok: true,
                    output: "1 处替换".to_string(),
                },
                ReplayEntry::Text {
                    text: "改好了。".to_string()
                },
            ]
        );

        // A turn without a stored transcript still replays its reply.
        store.start_turn("t2", "再问一句", 999_999).unwrap();
        store.complete_turn("t2", "好的。", None).unwrap();
        let replays = store.session_replay(5).unwrap();
        assert_eq!(replays.len(), 2);
        assert!(replays[1].entries.is_empty());
        assert_eq!(replays[1].assistant_content, "好的。");
        // Oldest first, so the caller can print them top to bottom.
        assert_eq!(replays[0].display_content, "改一下 README");
        assert!(replays.iter().all(|replay| !replay.is_job_wake));

        // A background-job wake turn is daemon-synthesized: the replay must be
        // able to tell it apart so it is not drawn as something the user typed.
        store
            .start_turn_with_display(
                "t3",
                "<background-job-report>子代理「后台测试A」已执行完毕</background-job-report>",
                "[后台任务完成] 子代理完成 82bea3 · 后台测试A",
                999_999,
                None,
            )
            .unwrap();
        store.complete_turn("t3", "跑完了。", None).unwrap();
        let replays = store.session_replay(5).unwrap();
        assert_eq!(replays.len(), 3);
        assert!(replays[2].is_job_wake);
        assert_eq!(
            replays[2].display_content,
            "[后台任务完成] 子代理完成 82bea3 · 后台测试A"
        );
    }

    #[test]
    fn one_shot_sessions_stay_invisible_and_stale_ones_are_swept() {
        let (temp, store) = test_store();
        store.init_files().unwrap();
        let user = store
            .create_session("miyu", "real", USER_SESSION_KIND, None)
            .unwrap();
        let ask = store
            .create_session("miyu", "一次性对话", ASK_SESSION_KIND, None)
            .unwrap();

        // Never listed, never findable by name — only the client holding the
        // freshly minted id can address it.
        let listed = store.list_sessions("miyu", true).unwrap();
        assert!(listed
            .iter()
            .any(|overview| overview.record.session_id == user.session_id));
        assert!(listed
            .iter()
            .all(|overview| overview.record.session_id != ask.session_id));
        assert!(store
            .find_local_session_by_name("miyu", "一次性对话")
            .unwrap()
            .is_none());

        // Fresh one-shot survives the sweep; an hour-old orphan does not.
        assert_eq!(store.delete_ask_sessions_older_than(1).unwrap(), 0);
        {
            use rusqlite::params;
            let backdated = (chrono::Utc::now() - chrono::Duration::hours(4)).to_rfc3339();
            let db_path = temp.path().join("state").join("conversation.db");
            let conn = rusqlite::Connection::open(db_path).unwrap();
            conn.execute("UPDATE sessions SET updated_at = ?1", params![backdated])
                .unwrap();
        }
        assert_eq!(store.delete_ask_sessions_older_than(1).unwrap(), 1);
        assert!(store.session_record(&ask.session_id).unwrap().is_none());
        // The equally backdated user session is untouched.
        assert!(store.session_record(&user.session_id).unwrap().is_some());
    }

    #[test]
    fn repl_session_pointer_is_separate_and_drops_when_stale() {
        let (_temp, store) = test_store();
        store.init_files().unwrap();
        let terminal = store.session_id().to_string();
        let repl = store
            .create_session("miyu", "repl lane", USER_SESSION_KIND, None)
            .unwrap();

        assert!(store.repl_session("miyu").unwrap().is_none());
        store.set_repl_session("miyu", &repl.session_id).unwrap();
        assert_eq!(
            store.repl_session("miyu").unwrap().as_deref(),
            Some(repl.session_id.as_str())
        );
        // Moving the REPL lane must not drag the terminal lane along.
        assert_eq!(&*store.session_id(), terminal.as_str());

        // Archived, then deleted: both make the pointer stale rather than
        // returning a session the REPL must not land on.
        store.set_session_archived(&repl.session_id, true).unwrap();
        assert!(store.repl_session("miyu").unwrap().is_none());
        store.set_session_archived(&repl.session_id, false).unwrap();
        assert!(store.repl_session("miyu").unwrap().is_some());
        store.delete_session(&repl.session_id).unwrap();
        assert!(store.repl_session("miyu").unwrap().is_none());
    }

    #[test]
    fn image_assets_persist_with_metadata_and_are_removed_with_history() {
        let (temp, store) = test_store();
        store.init_files().unwrap();
        store.start_turn("turn_image", "show it", 999999).unwrap();
        let path = temp.path().join("sample.png");
        image::RgbaImage::from_pixel(3, 2, image::Rgba([30, 120, 210, 255]))
            .save(&path)
            .unwrap();

        let saved = store
            .save_image_asset("turn_image", Some("tool_1"), &path, "sample image")
            .unwrap();
        assert_eq!(saved.mime, "image/png");
        assert_eq!((saved.width, saved.height), (3, 2));
        assert_eq!(store.load_image_assets().unwrap(), vec![saved.clone()]);
        let loaded = store.load_image_asset(&saved.asset_id).unwrap().unwrap();
        assert_eq!(loaded.asset, saved);
        assert!(!loaded.bytes.is_empty());

        store.reset_conversation().unwrap();
        assert!(store.load_image_assets().unwrap().is_empty());
        assert!(store
            .load_image_asset(&loaded.asset.asset_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn artifact_assets_update_in_place_and_are_removed_with_history() {
        let (temp, store) = test_store();
        store.init_files().unwrap();
        store
            .start_turn("turn_artifact", "build it", 999999)
            .unwrap();
        let path = temp.path().join("report.md");
        std::fs::write(&path, "# First\n").unwrap();
        let managed_dir = temp
            .path()
            .join("data/artifacts")
            .join(store.session_id().as_ref());
        std::fs::create_dir_all(&managed_dir).unwrap();
        std::fs::write(managed_dir.join("managed.md"), "# Managed\n").unwrap();

        let first = store
            .save_artifact_asset("turn_artifact", Some("tool_1"), &path, "Report")
            .unwrap();
        assert_eq!(first.kind, "markdown");
        assert_eq!(first.file_name, "Report");

        std::fs::write(&path, "# Updated\n").unwrap();
        let updated = store
            .save_artifact_asset("turn_artifact", Some("tool_2"), &path, "Updated report")
            .unwrap();
        assert_eq!(updated.asset_id, first.asset_id);
        assert_eq!(store.load_artifact_assets().unwrap(), vec![updated.clone()]);
        let loaded = store
            .load_artifact_asset(&updated.asset_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.bytes, b"# Updated\n");

        store.reset_conversation().unwrap();
        assert!(!managed_dir.exists());
        assert!(store.load_artifact_assets().unwrap().is_empty());
        assert!(store
            .load_artifact_asset(&updated.asset_id)
            .unwrap()
            .is_none());
    }

    #[test]
    fn managed_artifact_keeps_its_identity_across_turns() {
        let (temp, store) = test_store();
        store.init_files().unwrap();
        let managed_dir = temp
            .path()
            .join("data/artifacts")
            .join(store.session_id().as_ref());
        std::fs::create_dir_all(&managed_dir).unwrap();
        let path = managed_dir.join("report.md");

        store.start_turn("turn_one", "first", 999999).unwrap();
        std::fs::write(&path, "# First\n").unwrap();
        let first = store
            .save_artifact_asset("turn_one", Some("tool_one"), &path, "Report")
            .unwrap();
        store.complete_turn("turn_one", "done", None).unwrap();

        store.start_turn("turn_two", "update", 999999).unwrap();
        std::fs::write(&path, "# Updated\n").unwrap();
        let updated = store
            .save_artifact_asset("turn_two", Some("tool_two"), &path, "Report")
            .unwrap();

        assert_eq!(updated.asset_id, first.asset_id);
        assert_eq!(updated.turn_id, "turn_two");
        assert_eq!(store.load_artifact_assets().unwrap(), vec![updated.clone()]);
        assert_eq!(
            store
                .load_artifact_asset(&updated.asset_id)
                .unwrap()
                .unwrap()
                .bytes,
            b"# Updated\n"
        );
    }

    #[test]
    fn clearing_pinned_session_content_is_isolated_and_preserves_usage_and_binding() {
        let (_temp, store) = test_store();
        let current_session = store.session_id();
        store
            .start_turn("local_turn", "local prompt", std::process::id())
            .unwrap();
        store
            .complete_turn("local_turn", "local answer", None)
            .unwrap();

        let target_record = store
            .create_session("miyu", "qq:10000:private:42", "user", None)
            .unwrap();
        let target = store.pinned(&target_record.session_id);
        target
            .start_turn("qq_turn", "QQ prompt", std::process::id())
            .unwrap();
        target.complete_turn("qq_turn", "QQ answer", None).unwrap();
        target
            .enqueue_prompt("qq_queue", "queued", "queued", &[])
            .unwrap();
        let binding = platform_binding_key("42", None, "miyu");
        store
            .bind_platform_session(&binding, &target_record.session_id)
            .unwrap();

        store
            .add_usage(&Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Usage::default()
            })
            .unwrap();
        let usage_before = store.usage_snapshot().unwrap();

        target.clear_session_content().unwrap();

        assert!(target.load_turns().unwrap().is_empty());
        assert!(target.load_queued_prompts().unwrap().is_empty());
        assert_eq!(store.load_turns().unwrap().len(), 1);
        assert_eq!(store.session_id(), current_session);
        assert!(store
            .session_record(&target_record.session_id)
            .unwrap()
            .is_some());
        assert_eq!(
            store.find_platform_session_binding(&binding).unwrap(),
            Some(target_record.session_id)
        );
        let usage_after = store.usage_snapshot().unwrap();
        assert_eq!(usage_after.total_tokens, usage_before.total_tokens);
        assert_eq!(
            usage_after.conversation_tokens,
            usage_before.conversation_tokens
        );
    }

    /// Returns (non-summary fold ids, all visible ids) mirroring what the
    /// compactor passes for a full fold of the current history.
    fn visible_snapshot(store: &StateStore) -> (Vec<String>, Vec<String>) {
        let turns = store.load_visible_turns().unwrap();
        let fold_ids = turns
            .iter()
            .filter(|turn| !turn.is_summary)
            .map(|turn| turn.turn_id.clone())
            .collect();
        let turn_ids = turns.into_iter().map(|turn| turn.turn_id).collect();
        (fold_ids, turn_ids)
    }

    #[test]
    fn queued_prompts_persist_and_attach_to_a_turn_in_order() {
        let (_temp, store) = test_store();
        let first = store
            .enqueue_prompt(
                "q1",
                "first expanded",
                "first",
                &[QueuedPromptAttachment::Path {
                    path: "/tmp/image.png".to_string(),
                }],
            )
            .unwrap();
        let second = store
            .enqueue_prompt("q2", "second expanded", "second", &[])
            .unwrap();

        assert!(first.seq < second.seq);
        assert_eq!(
            store.load_queued_prompts().unwrap(),
            vec![first.clone(), second]
        );

        store.start_turn("t1", "initial", 999999).unwrap();
        store
            .consume_queued_prompts(
                "t1",
                &[
                    ("q1".to_string(), "first context".to_string()),
                    ("q2".to_string(), "second context".to_string()),
                ],
                Some("before followup"),
                Some("reasoning before followup"),
            )
            .unwrap();
        store.complete_turn("t1", "final answer", None).unwrap();

        assert!(store.load_queued_prompts().unwrap().is_empty());
        let turns = store.load_turns().unwrap();
        assert_eq!(turns[0].followups.len(), 2);
        assert_eq!(turns[0].followups[0].content, "first context");
        assert_eq!(turns[0].followups[0].attachments, first.attachments);
        assert_eq!(
            turns[0].followups[0]
                .preceding_assistant_reasoning
                .as_deref(),
            Some("reasoning before followup")
        );
        assert!(turns[0].followups[1].preceding_assistant_content.is_none());

        let history = store.load_conversation().unwrap();
        assert_eq!(
            history
                .iter()
                .map(|entry| (entry.role.as_str(), entry.content.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("user", "initial"),
                ("assistant", "before followup"),
                ("user", "first context"),
                ("user", "second context"),
                ("assistant", "final answer"),
            ]
        );

        store
            .enqueue_prompt("q3", "still queued", "still queued", &[])
            .unwrap();
        store.reset_conversation().unwrap();
        assert!(store.load_queued_prompts().unwrap().is_empty());
    }

    #[test]
    fn running_turn_exposes_its_queue_as_a_cross_process_target() {
        let (temp, owner_store) = test_store();
        owner_store
            .start_turn("running", "still working", std::process::id())
            .unwrap();
        let web_store = StateStore::new(&test_paths(temp.path())).unwrap();

        let target = web_store.running_turn_queue_target().unwrap().unwrap();
        assert_eq!(target.turn_id, "running");
        assert!(target.queue_session_id.is_some());
        assert_eq!(target.owner_pid, Some(std::process::id()));

        let queued = web_store
            .enqueue_prompt_for_target(&target, "followup", "next", "next", &[])
            .unwrap();
        assert_eq!(owner_store.load_queued_prompts().unwrap(), vec![queued]);
    }

    #[test]
    fn independent_process_stores_can_append_and_read_running_turns() {
        let (temp, first_store) = test_store();
        let second_store = StateStore::new(&test_paths(temp.path())).unwrap();

        first_store
            .start_turn("first", "first prompt", std::process::id())
            .unwrap();
        second_store
            .start_turn("second", "second prompt", std::process::id())
            .unwrap();

        let turns = first_store.load_visible_turns().unwrap();
        assert_eq!(turns.len(), 2);
        assert!(turns.iter().all(|turn| turn.status == TurnStatus::Running));
        assert!(turns
            .iter()
            .all(|turn| turn.assistant_content == pending_placeholder()));
    }

    #[test]
    fn queued_prompts_survive_prompt_changes_but_not_a_new_store_session() {
        let (temp, store) = test_store();
        store.reset_if_prompt_changed("system prompt one").unwrap();
        store
            .enqueue_prompt("q1", "queued content", "queued", &[])
            .unwrap();
        store.reset_if_prompt_changed("system prompt two").unwrap();
        assert_eq!(store.load_queued_prompts().unwrap().len(), 1);
        drop(store);

        let paths = MiyuPaths {
            root_dir: temp.path().to_path_buf(),
            config_dir: temp.path().join("config"),
            config_file: temp.path().join("config/config.jsonc"),
            skills_dir: temp.path().join("config/skills"),
            data_dir: temp.path().join("data"),
            cache_dir: temp.path().join("cache"),
            state_dir: temp.path().join("state"),
            pictures_dir: temp.path().join("pictures"),
            fish_hook_file: temp.path().join("fish/miyu.fish"),
            bash_hook_file: temp.path().join("shell/bash-hook.sh"),
            zsh_hook_file: temp.path().join("shell/zsh-hook.zsh"),
            scripts_dir: temp.path().join("config/scripts"),
            system_scripts_dir: PathBuf::new(),
        };
        let reopened = StateStore::new(&paths).unwrap();
        assert!(reopened.load_queued_prompts().unwrap().is_empty());
    }

    #[test]
    fn prompt_fingerprint_changes_never_delete_history() {
        let (_temp, store) = test_store();
        store
            .reset_if_prompt_changed("persona plus owner identity")
            .unwrap();
        store
            .start_turn("turn", "hello", std::process::id())
            .unwrap();
        store.complete_turn("turn", "reply", None).unwrap();

        store
            .reset_if_prompt_changed_with_compatible(
                "persona only",
                Some("persona plus owner identity"),
            )
            .unwrap();
        assert_eq!(store.load_visible_turns().unwrap().len(), 1);

        // v7 Release 3: a prompt text change is a planned cache cold start and
        // must never destroy conversation data.
        store.reset_if_prompt_changed("different persona").unwrap();
        assert_eq!(store.load_visible_turns().unwrap().len(), 1);
    }

    #[test]
    fn prompt_fingerprints_are_isolated_per_session() {
        let (_temp, store) = test_store();
        let first = store
            .create_session("first", "first", "user", None)
            .unwrap();
        let second = store
            .create_session("second", "second", "user", None)
            .unwrap();
        let first_store = store.pinned(&first.session_id);
        let second_store = store.pinned(&second.session_id);
        first_store.reset_if_prompt_changed("prompt A").unwrap();
        second_store.reset_if_prompt_changed("prompt B").unwrap();
        first_store
            .start_turn("first-turn", "hello", std::process::id())
            .unwrap();
        first_store
            .complete_turn("first-turn", "first reply", None)
            .unwrap();
        second_store
            .start_turn("second-turn", "hello", std::process::id())
            .unwrap();
        second_store
            .complete_turn("second-turn", "second reply", None)
            .unwrap();

        first_store.reset_if_prompt_changed("prompt A").unwrap();
        second_store.reset_if_prompt_changed("prompt B").unwrap();

        assert_eq!(first_store.load_visible_turns().unwrap().len(), 1);
        assert_eq!(second_store.load_visible_turns().unwrap().len(), 1);
    }

    #[test]
    fn stale_queue_cleanup_preserves_another_live_process_session() {
        let (_temp, store) = test_store();
        let live_owner = std::process::id();
        store
            .conv_db
            .enqueue_prompt(
                &store.session_id(),
                None,
                "other-q",
                "content",
                "display",
                &[],
                &[],
                "other-session",
                live_owner,
            )
            .unwrap();
        let different_pid = live_owner.wrapping_add(1).max(1);

        assert_eq!(
            store
                .conv_db
                .discard_stale_queued_prompts("new-session", different_pid)
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .conv_db
                .load_queued_prompts(&store.session_id(), "other-session")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn normal_session_cleanup_discards_unsent_prompts() {
        let (_temp, store) = test_store();
        store
            .enqueue_prompt("q1", "content", "display", &[])
            .unwrap();

        assert_eq!(store.discard_queued_prompts().unwrap(), 1);
        assert!(store.load_queued_prompts().unwrap().is_empty());
    }

    #[test]
    fn hidden_turns_excluded_from_visible() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply1", None).unwrap();
        store.start_turn("t2", "second", 999999).unwrap();
        store.complete_turn("t2", "reply2", None).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);

        let hidden_count = store.hide_turns_before_seq(visible[0].seq).unwrap();
        assert_eq!(hidden_count, 1);

        let visible_after = store.load_visible_turns().unwrap();
        assert_eq!(visible_after.len(), 1);
        assert_eq!(visible_after[0].turn_id, "t2");

        let all = store.load_turns().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].hidden);
        assert!(!all[1].hidden);
    }

    #[test]
    fn summary_turn_insert_and_load() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "hi", None).unwrap();

        store
            .insert_summary_turn(
                "## Task Goal\nDo stuff",
                TurnTokens {
                    total: 12,
                    ..Default::default()
                },
                true,
            )
            .unwrap();

        let summary = store.load_last_summary().unwrap();
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert!(summary.is_summary);
        assert!(!summary.hidden);
        assert_eq!(summary.assistant_content, "## Task Goal\nDo stuff");
        assert_eq!(summary.token_total, 12);
        assert!(summary.token_usage_estimated);

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert!(visible.iter().any(|t| t.is_summary));
        assert!(visible.iter().any(|t| !t.is_summary));
    }

    #[test]
    fn session_loaded_tools_persist_until_reset() {
        let (_temp, store) = test_store();
        store
            .add_session_loaded_tools(&["web_search".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_targets(&["group:gaming".to_string()], Some("t1"))
            .unwrap();

        let loaded = store.load_session_loaded_tools().unwrap();
        assert!(loaded.contains("web_search"));

        store.reset_conversation().unwrap();
        assert!(store.load_session_loaded_tools().unwrap().is_empty());
    }

    #[test]
    fn hide_before_seq_hides_old_summary_too() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "old", 999999).unwrap();
        store.complete_turn("t1", "old reply", None).unwrap();
        store
            .insert_summary_turn(
                "summary of old",
                TurnTokens {
                    total: 8,
                    ..Default::default()
                },
                true,
            )
            .unwrap();
        store.start_turn("t2", "new", 999999).unwrap();
        store.complete_turn("t2", "new reply", None).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 3);

        let t2_seq = visible.last().unwrap().seq;
        let hidden = store.hide_turns_before_seq(t2_seq).unwrap();
        assert_eq!(hidden, 3);

        let visible_after = store.load_visible_turns().unwrap();
        assert!(visible_after.is_empty());
    }

    #[test]
    fn evictable_turns_are_deleted_only_after_explicit_commit() {
        let (_temp, store) = test_store();
        for i in 0..10 {
            let id = format!("t{i}");
            let content = "x".repeat(1000);
            store.start_turn(&id, &content, 999999).unwrap();
            store.complete_turn(&id, &content, None).unwrap();
        }

        let evicted = store.oldest_evictable_visible_turns(3).unwrap();
        assert_eq!(evicted.len(), 3);
        assert_eq!(store.load_visible_turns().unwrap().len(), 10);

        let ids = evicted
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>();
        store.delete_visible_turns(&ids).unwrap();

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 7);
    }

    #[test]
    fn deleting_no_visible_turns_is_a_noop() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "short", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();

        assert_eq!(store.delete_visible_turns(&[]).unwrap(), 0);

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
    }

    #[test]
    fn deleting_visible_turns_rolls_back_when_any_id_changed() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["from_t1".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_tools(&["from_t2".to_string()], Some("t2"))
            .unwrap();

        assert!(store
            .delete_visible_turns(&["t1".to_string(), "missing".to_string()])
            .is_err());
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
        assert_eq!(
            store.load_session_loaded_tools().unwrap(),
            BTreeSet::from(["from_t1".to_string(), "from_t2".to_string()])
        );
    }

    #[test]
    fn checked_pop_rolls_back_when_loaded_tool_sources_change() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t1"))
            .unwrap();
        let expected = store.load_session_loaded_tools_with_sources().unwrap();
        store
            .add_session_loaded_tools(&["dynamic_tool".to_string()], Some("t2"))
            .unwrap();

        assert!(store
            .delete_visible_turns_checked(&["t1".to_string()], Some(&expected))
            .is_err());

        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
        assert_eq!(
            store.load_session_loaded_tools_with_sources().unwrap(),
            vec![("dynamic_tool".to_string(), Some("t2".to_string()))]
        );
    }

    #[test]
    fn deleting_visible_turns_unloads_only_items_sourced_from_deleted_turns() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        store
            .add_session_loaded_tools(&["popped_tool".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_tools(&["kept_tool".to_string()], Some("t2"))
            .unwrap();
        store
            .add_session_loaded_tools(&["global_tool".to_string()], None)
            .unwrap();
        store
            .add_session_loaded_targets(&["popped_target".to_string()], Some("t1"))
            .unwrap();
        store
            .add_session_loaded_targets(&["kept_target".to_string()], Some("t2"))
            .unwrap();

        assert_eq!(store.delete_visible_turns(&["t1".to_string()]).unwrap(), 1);

        assert_eq!(
            store.load_session_loaded_tools().unwrap(),
            BTreeSet::from(["global_tool".to_string(), "kept_tool".to_string()])
        );
        assert_eq!(
            store
                .conv_db
                .load_session_loaded_items(&store.session_id(), "target")
                .unwrap(),
            BTreeSet::from(["kept_target".to_string()])
        );
    }

    #[test]
    fn interrupted_turn_is_evictable_but_summary_and_running_turn_are_not() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn(
                "summary",
                TurnTokens {
                    total: 1,
                    ..Default::default()
                },
                false,
            )
            .unwrap();
        store.start_turn("completed", "completed", 999999).unwrap();
        store.complete_turn("completed", "reply", None).unwrap();
        store
            .start_turn("interrupted", "interrupted", 999999)
            .unwrap();
        store.interrupt_turn("interrupted").unwrap();
        store
            .start_turn("running", "pending", std::process::id())
            .unwrap();

        let evicted = store.oldest_evictable_visible_turns(10).unwrap();
        assert_eq!(
            evicted
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["completed", "interrupted"]
        );
        assert_eq!(evicted[1].status, TurnStatus::Interrupted);
    }

    #[test]
    fn compact_is_reversible_with_undo() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (fold_ids, turn_ids) = visible_snapshot(&store);

        store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "summary",
                TurnTokens {
                    total: 10,
                    ..Default::default()
                },
                true,
                None,
            )
            .unwrap();

        let all = store.load_turns().unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].hidden && all[1].hidden);
        assert_eq!(store.load_visible_turns().unwrap().len(), 1);
        assert_eq!(
            store
                .load_conversation()
                .unwrap()
                .into_iter()
                .filter(|entry| entry.role == "user")
                .map(|entry| entry.content)
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );

        let (removed, prompt) = store.undo_last_turn().unwrap();
        assert_eq!(removed, 1);
        assert!(prompt.is_none());
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(
            visible
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
    }

    #[test]
    fn nested_compact_undo_restores_one_layer_at_a_time() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (fold_ids, turn_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "summary one",
                TurnTokens::default(),
                false,
                None,
            )
            .unwrap();
        store.start_turn("t3", "third", 999999).unwrap();
        store.complete_turn("t3", "reply", None).unwrap();
        let (fold_ids, turn_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "summary two",
                TurnTokens::default(),
                false,
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "summary two"
        );
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].assistant_content, "summary one");
        assert_eq!(visible[1].turn_id, "t3");

        assert_eq!(store.undo_last_turn().unwrap().1.as_deref(), Some("third"));
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(
            visible
                .iter()
                .map(|turn| turn.turn_id.as_str())
                .collect::<Vec<_>>(),
            vec!["t1", "t2"]
        );
    }

    #[test]
    fn tail_retention_compact_folds_only_the_selected_turns() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2", "t3", "t4"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (_, all_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(
                &["t1".to_string(), "t2".to_string()],
                &all_ids,
                "summary",
                TurnTokens::default(),
                false,
                None,
            )
            .unwrap();

        let visible = store.load_visible_turns().unwrap();
        let ids: Vec<&str> = visible.iter().map(|t| t.turn_id.as_str()).collect();
        assert_eq!(&ids[..2], &["t3", "t4"]);
        assert_eq!(visible.len(), 3);
        assert!(visible[2].is_summary);
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "summary"
        );

        // Undo restores exactly the folded set and deletes the summary.
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(
            visible.iter().map(|t| t.turn_id.as_str()).collect::<Vec<_>>(),
            vec!["t1", "t2", "t3", "t4"]
        );
    }

    #[test]
    fn second_tail_compact_supersedes_the_previous_summary() {
        let (_temp, store) = test_store();
        for id in ["t1", "t2", "t3"] {
            store.start_turn(id, id, 999999).unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }
        let (_, all_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(
                &["t1".to_string()],
                &all_ids,
                "summary one",
                TurnTokens::default(),
                false,
                None,
            )
            .unwrap();
        store.start_turn("t4", "fourth", 999999).unwrap();
        store.complete_turn("t4", "reply", None).unwrap();

        // Second compaction folds t2 (oldest visible non-summary turn); the
        // superseded summary must be hidden together with it even though its
        // seq is higher than the tail turns'.
        let (_, all_ids) = visible_snapshot(&store);
        store
            .replace_visible_with_summary(
                &["t2".to_string()],
                &all_ids,
                "summary two",
                TurnTokens::default(),
                false,
                None,
            )
            .unwrap();

        let visible = store.load_visible_turns().unwrap();
        let ids: Vec<&str> = visible.iter().map(|t| t.turn_id.as_str()).collect();
        assert_eq!(&ids[..2], &["t3", "t4"]);
        assert_eq!(visible.len(), 3);
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "summary two"
        );
        assert_eq!(
            visible.iter().filter(|t| t.is_summary).count(),
            1,
            "the superseded summary must not stay visible"
        );

        // Undo restores t2 and summary one, drops summary two.
        assert_eq!(store.undo_last_turn().unwrap(), (1, None));
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "summary one"
        );
        let visible = store.load_visible_turns().unwrap();
        assert!(visible.iter().any(|t| t.turn_id == "t2" && !t.hidden));
    }

    #[test]
    fn prune_folds_old_tool_reports_behind_the_harvest_gate() {
        let (_temp, store) = test_store();
        let big_report = "x".repeat(4096);
        for id in ["t1", "t2", "t3", "t4"] {
            store.start_turn(id, id, 999999).unwrap();
            store
                .conv_db
                .append_tool_reports(id, &[big_report.clone()])
                .unwrap();
            store.complete_turn(id, "reply", None).unwrap();
        }

        // Harvest gate: potential savings (~8KB from t1+t2) below the
        // threshold → nothing is rewritten.
        let stats = store.prune_stale_tool_reports(2, 1_000_000).unwrap();
        assert_eq!(stats.turns, 0);
        let turns = store.load_visible_turns().unwrap();
        assert_eq!(turns[0].tool_reports[0], big_report);

        // Gate passes: the two oldest turns fold, newest two are protected.
        let stats = store.prune_stale_tool_reports(2, 1024).unwrap();
        assert_eq!(stats.turns, 2);
        assert!(stats.saved_chars > 6000);
        let turns = store.load_visible_turns().unwrap();
        assert!(turns[0].tool_reports[0].contains("已折叠"));
        assert!(turns[1].tool_reports[0].contains("已折叠"));
        assert_eq!(turns[2].tool_reports[0], big_report);
        assert_eq!(turns[3].tool_reports[0], big_report);

        // Monotonic: a second pass finds nothing new to rewrite (the
        // archived turns are never re-pruned, so the cache is not re-hit).
        let stats = store.prune_stale_tool_reports(2, 1024).unwrap();
        assert_eq!(stats.turns, 0);
    }

    #[test]
    fn empty_summary_leaves_visible_turns_unchanged() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (fold_ids, turn_ids) = visible_snapshot(&store);

        assert!(store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "  ",
                TurnTokens::default(),
                false,
                None
            )
            .is_err());

        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].turn_id, "t1");
    }

    #[test]
    fn compact_insert_failure_rolls_back_hidden_turns() {
        let (temp, store) = test_store();
        store.start_turn("t1", "hello", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (fold_ids, turn_ids) = visible_snapshot(&store);
        let conn = rusqlite::Connection::open(temp.path().join("state/conversation.db")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_summary_insert
             BEFORE INSERT ON turns WHEN NEW.is_summary = 1
             BEGIN SELECT RAISE(ABORT, 'injected summary failure'); END;",
        )
        .unwrap();

        assert!(store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "summary",
                TurnTokens::default(),
                false,
                None
            )
            .is_err());
        let visible = store.load_visible_turns().unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].turn_id, "t1");
        assert!(!visible[0].hidden);
    }

    #[test]
    fn irreversible_legacy_summary_is_not_deleted_by_undo() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn("legacy summary", TurnTokens::default(), false)
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "legacy summary"
        );
    }

    #[test]
    fn irreversible_nested_legacy_summary_is_not_downgraded_by_undo() {
        let (_temp, store) = test_store();
        store
            .insert_summary_turn("legacy summary one", TurnTokens::default(), false)
            .unwrap();
        let first_seq = store.load_visible_turns().unwrap()[0].seq;
        store.hide_turns_before_seq(first_seq).unwrap();
        store
            .insert_summary_turn("legacy summary two", TurnTokens::default(), false)
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(
            store
                .load_last_summary()
                .unwrap()
                .unwrap()
                .assistant_content,
            "legacy summary two"
        );
    }

    #[test]
    fn undo_does_not_remove_a_running_turn() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "completed", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        store
            .start_turn("running", "active", std::process::id())
            .unwrap();

        assert_eq!(store.undo_last_turn().unwrap(), (0, None));
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    }

    #[test]
    fn compact_rejects_a_changed_snapshot() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (fold_ids, turn_ids) = visible_snapshot(&store);
        store.undo_last_turn().unwrap();

        assert!(store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "stale",
                TurnTokens::default(),
                false,
                None
            )
            .is_err());
        assert!(store.load_visible_turns().unwrap().is_empty());
    }

    #[test]
    fn compact_rejects_a_new_turn_after_snapshot() {
        let (_temp, store) = test_store();
        store.start_turn("t1", "first", 999999).unwrap();
        store.complete_turn("t1", "reply", None).unwrap();
        let (fold_ids, turn_ids) = visible_snapshot(&store);
        store.start_turn("t2", "second", 999999).unwrap();
        store.complete_turn("t2", "reply", None).unwrap();

        assert!(store
            .replace_visible_with_summary(
                &fold_ids,
                &turn_ids,
                "stale",
                TurnTokens::default(),
                false,
                None
            )
            .is_err());
        assert_eq!(store.load_visible_turns().unwrap().len(), 2);
    }

    #[test]
    fn initial_prompt_redo_reuses_the_turn_with_a_new_revision() {
        let (_temp, store) = test_store();
        store
            .start_turn_with_display("t1", "original", "original", 999999, None)
            .unwrap();
        store.complete_turn("t1", "old answer", None).unwrap();

        let candidate = store.redo_candidate().unwrap().unwrap();
        assert_eq!(candidate.input_kind, RedoInputKind::Initial);
        let redo = store
            .begin_redo(
                "t1",
                "t1",
                RedoInputKind::Initial,
                candidate.revision,
                "edited internal",
                "edited",
                std::process::id(),
            )
            .unwrap();
        assert_eq!(redo.revision, 1);
        assert!(redo.checkpoint.is_none());

        let turn = store.load_turns().unwrap().remove(0);
        assert_eq!(turn.revision, 1);
        assert_eq!(turn.status, TurnStatus::Running);
        assert_eq!(turn.user_content, "edited internal");
        assert_eq!(turn.display_content, "edited");
        assert!(store
            .begin_redo(
                "t1",
                "t1",
                RedoInputKind::Initial,
                candidate.revision,
                "stale",
                "stale",
                std::process::id(),
            )
            .is_err());

        store
            .complete_turn_revision_with_usage_and_model(
                "t1",
                1,
                "new answer",
                None,
                None,
                None,
                TurnTokens::default(),
                false,
            )
            .unwrap();
        assert_eq!(
            store.load_turns().unwrap()[0].assistant_content,
            "new answer"
        );
    }

    #[test]
    fn followup_redo_restores_the_last_batch_checkpoint() {
        let (_temp, store) = test_store();
        store
            .start_turn("t1", "initial", std::process::id())
            .unwrap();
        store
            .enqueue_prompt("q1", "followup", "followup", &[])
            .unwrap();
        let checkpoint = TurnRedoCheckpointPayload {
            replay_messages: vec![crate::llm::ChatMessage::plain("assistant", "prefix answer")],
            prefix_tool_reports: vec!["prefix report".to_string()],
            tool_rounds: 1,
            question_rounds: 0,
            loaded_items: Vec::new(),
            prefix_question_count: 0,
            prefix_image_asset_ids: Vec::new(),
            prefix_artifact_asset_ids: Vec::new(),
        };
        store
            .consume_queued_prompts_with_checkpoint(
                "t1",
                &[("q1".to_string(), "followup".to_string())],
                Some("prefix answer"),
                None,
                None,
                None,
                checkpoint,
            )
            .unwrap();
        store.complete_turn("t1", "old final", None).unwrap();

        let candidate = store.redo_candidate().unwrap().unwrap();
        assert_eq!(candidate.input_kind, RedoInputKind::Followup);
        assert_eq!(candidate.input_id, "q1");
        let redo = store
            .begin_redo(
                "t1",
                "q1",
                RedoInputKind::Followup,
                candidate.revision,
                "edited followup",
                "edited followup",
                std::process::id(),
            )
            .unwrap();
        let redo_revision = redo.revision;
        let checkpoint = redo.checkpoint.unwrap();
        assert_eq!(checkpoint.replay_messages.len(), 1);
        assert_eq!(checkpoint.prefix_tool_reports, vec!["prefix report"]);
        let turn = store.load_turns().unwrap().remove(0);
        assert_eq!(turn.followups[0].content, "edited followup");
        assert_eq!(turn.tool_reports, vec!["prefix report"]);
        store
            .enqueue_prompt("q2", "new during redo", "new during redo", &[])
            .unwrap();
        store
            .consume_queued_prompts_with_checkpoint(
                "t1",
                &[("q2".to_string(), "new during redo".to_string())],
                None,
                None,
                None,
                None,
                TurnRedoCheckpointPayload {
                    replay_messages: Vec::new(),
                    prefix_tool_reports: Vec::new(),
                    tool_rounds: 0,
                    question_rounds: 0,
                    loaded_items: Vec::new(),
                    prefix_question_count: 0,
                    prefix_image_asset_ids: Vec::new(),
                    prefix_artifact_asset_ids: Vec::new(),
                },
            )
            .unwrap();
        store.interrupt_turn_revision("t1", redo_revision).unwrap();
        let restored = store.load_turns().unwrap().remove(0);
        assert_eq!(restored.revision, 0);
        assert_eq!(restored.status, TurnStatus::Completed);
        assert_eq!(restored.assistant_content, "old final");
        assert_eq!(restored.followups[0].content, "followup");
        assert_eq!(restored.followups.len(), 1);
        assert_eq!(store.redo_candidate().unwrap().unwrap().input_id, "q1");
    }

    #[test]
    fn cancelled_initial_redo_restores_the_previous_turn() {
        let (_temp, store) = test_store();
        store
            .start_turn_with_display("t1", "internal", "visible", 999999, None)
            .unwrap();
        store
            .complete_turn("t1", "old answer", Some("old reasoning"))
            .unwrap();
        let candidate = store.redo_candidate().unwrap().unwrap();
        let redo = store
            .begin_redo(
                "t1",
                "t1",
                RedoInputKind::Initial,
                candidate.revision,
                "edited internal",
                "edited visible",
                std::process::id(),
            )
            .unwrap();

        store.interrupt_turn_revision("t1", redo.revision).unwrap();
        let restored = store.load_turns().unwrap().remove(0);
        assert_eq!(restored.revision, 0);
        assert_eq!(restored.status, TurnStatus::Completed);
        assert_eq!(restored.user_content, "internal");
        assert_eq!(restored.display_content, "visible");
        assert_eq!(restored.assistant_content, "old answer");
        assert_eq!(
            restored.assistant_reasoning.as_deref(),
            Some("old reasoning")
        );
    }

    #[test]
    fn cancelled_redo_restores_artifact_versions() {
        let (temp, store) = test_store();
        let artifact_dir = temp.path().join("data/artifacts/default");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let path = artifact_dir.join("report.md");
        std::fs::write(&path, "old artifact").unwrap();
        store
            .start_turn("t1", "create report", std::process::id())
            .unwrap();
        let old = store
            .save_artifact_asset("t1", Some("tool-old"), &path, "Report")
            .unwrap();
        store.complete_turn("t1", "old answer", None).unwrap();

        let candidate = store.redo_candidate().unwrap().unwrap();
        let redo = store
            .begin_redo(
                "t1",
                "t1",
                RedoInputKind::Initial,
                candidate.revision,
                "redo report",
                "redo report",
                std::process::id(),
            )
            .unwrap();
        assert!(store.load_artifact_assets().unwrap().is_empty());
        std::fs::write(&path, "new artifact").unwrap();
        store
            .save_artifact_asset("t1", Some("tool-new"), &path, "Report")
            .unwrap();
        store.interrupt_turn_revision("t1", redo.revision).unwrap();

        let restored = store.load_artifact_asset(&old.asset_id).unwrap().unwrap();
        assert_eq!(restored.asset.tool_id.as_deref(), Some("tool-old"));
        assert_eq!(restored.bytes, b"old artifact");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old artifact");
    }
}
