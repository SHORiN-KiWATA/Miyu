use super::super::{
    send_fixed_tool_output, TOOL_ADD_ACTIVE_JUDGEMENT_SKIP, TOOL_REMOVE_ACTIVE_JUDGEMENT_SKIP,
};
use crate::config::REAL_CONTEXT_PLUGIN_ID;
use crate::platforms::access_control::{
    administrator_authorization, is_effective_admin, ONEBOT_PLATFORM,
};
use crate::platforms::{PlatformInboundEventKind, PlatformTurnContext};
use crate::state::{PlatformPluginScopeKey, StateStore, GLOBAL_PLATFORM_ACCOUNT_SCOPE};
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const STATE_KEY: &str = "active_judgement_skip_user_ids";
const GLOBAL_CONVERSATION_KIND: &str = "global";
const MAX_SKIP_USER_IDS: usize = 10_000;
const MAX_CACHE_ENTRIES: usize = 128;
const CACHE_RECHECK_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct CachedIds {
    revision: Option<String>,
    ids: Arc<BTreeSet<i64>>,
    checked_at: Instant,
}

fn ids_cache() -> &'static Mutex<HashMap<PathBuf, CachedIds>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedIds>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy)]
enum Mutation {
    Add,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MutationOutcome {
    Added,
    AlreadyPresent,
    Removed,
    NotPresent,
}

fn state_scope() -> PlatformPluginScopeKey {
    PlatformPluginScopeKey {
        plugin_id: REAL_CONTEXT_PLUGIN_ID.to_string(),
        platform: ONEBOT_PLATFORM.to_string(),
        account_id: GLOBAL_PLATFORM_ACCOUNT_SCOPE.to_string(),
        conversation_kind: GLOBAL_CONVERSATION_KIND.to_string(),
        conversation_id: GLOBAL_PLATFORM_ACCOUNT_SCOPE.to_string(),
    }
}

fn validate_ids(ids: &BTreeSet<i64>) -> Result<()> {
    if ids.len() > MAX_SKIP_USER_IDS {
        bail!("active judgement skip list cannot exceed {MAX_SKIP_USER_IDS} QQ ids");
    }
    if ids.iter().any(|id| *id <= 0) {
        bail!("active judgement skip list contains an invalid QQ id");
    }
    Ok(())
}

fn load_ids(state: &StateStore) -> Result<BTreeSet<i64>> {
    let ids = state
        .plugin_get_json::<BTreeSet<i64>>(&state_scope(), STATE_KEY)?
        .unwrap_or_default();
    validate_ids(&ids)?;
    Ok(ids)
}

fn cached_ids(state: &StateStore) -> Result<Arc<BTreeSet<i64>>> {
    let cache_key = state.state_dir().to_path_buf();
    if let Some(ids) = ids_cache()
        .lock()
        .unwrap()
        .get(&cache_key)
        .filter(|cached| cached.checked_at.elapsed() < CACHE_RECHECK_INTERVAL)
        .map(|cached| cached.ids.clone())
    {
        return Ok(ids);
    }

    let scope = state_scope();
    let revision = state.plugin_json_revision(&scope, STATE_KEY)?;
    if let Some(ids) = ids_cache()
        .lock()
        .unwrap()
        .get_mut(&cache_key)
        .filter(|cached| cached.revision == revision)
        .map(|cached| {
            cached.checked_at = Instant::now();
            cached.ids.clone()
        })
    {
        return Ok(ids);
    }

    let snapshot = state.plugin_get_json_with_revision::<BTreeSet<i64>>(&scope, STATE_KEY)?;
    let (ids, revision) = snapshot
        .map(|(ids, revision)| (ids, Some(revision)))
        .unwrap_or_default();
    validate_ids(&ids)?;
    let ids = Arc::new(ids);
    let mut cache = ids_cache().lock().unwrap();
    if cache.len() >= MAX_CACHE_ENTRIES && !cache.contains_key(&cache_key) {
        cache.clear();
    }
    cache.insert(
        cache_key,
        CachedIds {
            revision,
            ids: ids.clone(),
            checked_at: Instant::now(),
        },
    );
    Ok(ids)
}

fn invalidate_cache(state: &StateStore) {
    ids_cache().lock().unwrap().remove(state.state_dir());
}

pub(crate) fn active_judgement_skip_ids(state: &StateStore) -> Result<Vec<i64>> {
    Ok(load_ids(state)?.into_iter().collect())
}

pub(super) fn contains(state: &StateStore, user_id: &str) -> Result<bool> {
    let user_id = user_id
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .context("QQ sender id must be a positive integer")?;
    Ok(cached_ids(state)?.contains(&user_id))
}

pub(crate) fn apply_active_judgement_skip_editor_changes(
    state: &StateStore,
    original: &[i64],
    edited: &[i64],
) -> Result<Vec<i64>> {
    let original = original.iter().copied().collect::<BTreeSet<_>>();
    let edited = edited.iter().copied().collect::<BTreeSet<_>>();
    validate_ids(&original)?;
    validate_ids(&edited)?;
    let additions = edited.difference(&original).copied().collect::<Vec<_>>();
    let removals = original.difference(&edited).copied().collect::<Vec<_>>();
    let updated = state
        .plugin_update_json(
            &state_scope(),
            STATE_KEY,
            move |current: Option<BTreeSet<i64>>| {
                let mut current = current.unwrap_or_default();
                validate_ids(&current)?;
                for id in removals {
                    current.remove(&id);
                }
                current.extend(additions);
                validate_ids(&current)?;
                Ok((!current.is_empty()).then_some(current))
            },
        )?
        .unwrap_or_default();
    invalidate_cache(state);
    Ok(updated.into_iter().collect())
}

fn mutate(state: &StateStore, user_id: i64, mutation: Mutation) -> Result<MutationOutcome> {
    if user_id <= 0 {
        bail!("qq_id must be a positive integer");
    }
    let mut outcome = MutationOutcome::NotPresent;
    state.plugin_update_json(
        &state_scope(),
        STATE_KEY,
        |current: Option<BTreeSet<i64>>| {
            let mut current = current.unwrap_or_default();
            validate_ids(&current)?;
            outcome = match mutation {
                Mutation::Add if current.insert(user_id) => MutationOutcome::Added,
                Mutation::Add => MutationOutcome::AlreadyPresent,
                Mutation::Remove if current.remove(&user_id) => MutationOutcome::Removed,
                Mutation::Remove => MutationOutcome::NotPresent,
            };
            validate_ids(&current)?;
            Ok((!current.is_empty()).then_some(current))
        },
    )?;
    invalidate_cache(state);
    Ok(outcome)
}

fn effective_admin(context: &PlatformTurnContext) -> bool {
    context.with_current_config(|config| {
        is_effective_admin(
            &config.platforms.qq,
            &context.state_store,
            &context.conversation.account_id,
            &context.sender_id,
        )
    })
}

fn require_live_message(context: &PlatformTurnContext) -> Result<()> {
    if context.conversation.platform != ONEBOT_PLATFORM {
        bail!("active judgement skip management requires OneBot");
    }
    let event = context
        .inbound_event()
        .context("active judgement skip management requires a live platform message")?;
    if event.kind != PlatformInboundEventKind::Message
        || event.sender_id != context.sender_id
        || event.conversation != context.conversation
    {
        bail!("active judgement skip administrator identity does not match the current message");
    }
    Ok(())
}

fn mutate_as_admin(
    context: &PlatformTurnContext,
    user_id: i64,
    mutation: Mutation,
) -> Result<MutationOutcome> {
    context
        .with_current_config(|config| {
            let authorization = administrator_authorization(
                &config.platforms.qq,
                &context.conversation.account_id,
                &context.sender_id,
            );
            context
                .state_store
                .with_platform_access_authorization(&authorization, || {
                    mutate(&context.state_store, user_id, mutation)
                })
        })?
        .context("active judgement skip management requires a current Miyu administrator")
}

fn qq_id(arguments: &Value) -> Result<i64> {
    arguments
        .get("qq_id")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
        .filter(|id| *id > 0)
        .context("qq_id must be a positive integer")
}

fn response_text(outcome: MutationOutcome, user_id: i64) -> String {
    match outcome {
        MutationOutcome::Added => format!("已加入跳过主动判断名单：{user_id}"),
        MutationOutcome::AlreadyPresent => format!("已在跳过主动判断名单中：{user_id}"),
        MutationOutcome::Removed => format!("已移出跳过主动判断名单：{user_id}"),
        MutationOutcome::NotPresent => format!("不在跳过主动判断名单中：{user_id}"),
    }
}

async fn run_tool(
    arguments: Value,
    context: Arc<PlatformTurnContext>,
    mutation: Mutation,
) -> Result<String> {
    let result = (|| {
        require_live_message(&context)?;
        let user_id = qq_id(&arguments)?;
        let outcome = mutate_as_admin(&context, user_id, mutation)?;
        Ok::<_, anyhow::Error>(response_text(outcome, user_id))
    })();
    let (message, operation_succeeded) = match result {
        Ok(message) => (message, true),
        Err(error) => {
            tracing::warn!(
                target: "miyu::qq",
                error = %error,
                sender_id = %context.sender_id,
                "active judgement skip tool request failed"
            );
            ("操作失败".to_string(), false)
        }
    };
    send_fixed_tool_output(&context, &message).await?;
    Ok(json!({
        "ok": operation_succeeded,
        "operation_succeeded": operation_succeeded,
        "message_sent": true,
        "do_not_send_another_reply": true
    })
    .to_string())
}

pub(super) fn register_tools(registry: &mut ToolRegistry, context: Arc<PlatformTurnContext>) {
    if context.conversation.platform != ONEBOT_PLATFORM || !effective_admin(&context) {
        return;
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "qq_id": {
                "type": ["string", "integer"],
                "description": "The positive QQ user id to manage."
            }
        },
        "required": ["qq_id"],
        "additionalProperties": false
    });
    let add_context = context.clone();
    registry.register(
        ToolSpec::new(
            TOOL_ADD_ACTIVE_JUDGEMENT_SKIP,
            "Add one QQ user to the global list that skips active-reply social judgement. Safety and moderation judgement remain enabled. The Rust host sends the final result, so do not send another acknowledgement.",
            schema.clone(),
            move |arguments| {
                let context = add_context.clone();
                async move { run_tool(arguments, context, Mutation::Add).await }
            },
        )
        .writes()
        .with_display_name("添加跳过主动判断 QQ 号"),
    );
    registry.register(
        ToolSpec::new(
            TOOL_REMOVE_ACTIVE_JUDGEMENT_SKIP,
            "Remove one QQ user from the global list that skips active-reply social judgement. The Rust host sends the final result, so do not send another acknowledgement.",
            schema,
            move |arguments| {
                let context = context.clone();
                async move { run_tool(arguments, context, Mutation::Remove).await }
            },
        )
        .writes()
        .with_display_name("移除跳过主动判断 QQ 号"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::paths::MiyuPaths;
    use crate::platforms::access_control::{global_grant_key, AccessPermission};
    use crate::platforms::plugins::PlatformPluginRegistry;
    use crate::platforms::{
        ConversationKind, OutboundBody, OutboundMessage, OutboundSegment, PlatformAdapter,
        PlatformConversation, PlatformInboundEvent, SendReceipt,
    };
    use crate::state::PlatformAccessActor;
    use crate::tools::ToolPermission;
    use futures_util::future::BoxFuture;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Instant;

    #[derive(Default)]
    struct RecordingAdapter {
        messages: Mutex<Vec<OutboundMessage>>,
    }

    impl PlatformAdapter for RecordingAdapter {
        fn send<'a>(&'a self, message: OutboundMessage) -> BoxFuture<'a, Result<SendReceipt>> {
            Box::pin(async move {
                self.messages.lock().unwrap().push(message);
                Ok(SendReceipt {
                    delivered_parts: 1,
                    ..SendReceipt::default()
                })
            })
        }

        fn bot_display_name<'a>(&'a self) -> BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }
    }

    fn test_paths(root: &std::path::Path) -> MiyuPaths {
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

    fn test_context(
        paths: &MiyuPaths,
        state: StateStore,
        adapter: Arc<RecordingAdapter>,
        static_admin: bool,
    ) -> Arc<PlatformTurnContext> {
        let conversation = PlatformConversation {
            platform: ONEBOT_PLATFORM.to_string(),
            account_id: "10000".to_string(),
            kind: ConversationKind::Private,
            conversation_id: "42".to_string(),
        };
        let mut config = AppConfig::default();
        if static_admin {
            config.platforms.qq.admin_users.push(42);
        }
        Arc::new(
            PlatformTurnContext::new(
                conversation.clone(),
                "42".to_string(),
                "admin".to_string(),
                static_admin,
                config,
                paths.clone(),
                state,
                adapter,
                Arc::new(PlatformPluginRegistry::default()),
            )
            .with_inbound_event(PlatformInboundEvent {
                kind: PlatformInboundEventKind::Message,
                conversation,
                conversation_display_name: None,
                message_id: "message-1".to_string(),
                sender_id: "42".to_string(),
                sender_display_name: "admin".to_string(),
                operator_id: None,
                timestamp: 1,
                received_at: Instant::now(),
                message_position: None,
                ingress_order: None,
                text: "修改主动判断名单".to_string(),
                reply_to_message_id: None,
                replied_message: None,
                mentioned_user_ids: Vec::new(),
                mentioned_users: Vec::new(),
                mentioned_bot: false,
                media: Vec::new(),
                notice_sub_type: None,
                duration_seconds: None,
            }),
        )
    }

    fn outbound_text(message: &OutboundMessage) -> String {
        let OutboundBody::Segments(segments) = &message.body else {
            panic!("expected text segments");
        };
        segments
            .iter()
            .filter_map(|segment| match segment {
                OutboundSegment::Text(text) | OutboundSegment::Markdown(text) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn mutations_are_idempotent_sorted_and_persistent() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();

        assert_eq!(
            mutate(&state, 20, Mutation::Add).unwrap(),
            MutationOutcome::Added
        );
        assert_eq!(
            mutate(&state, 10, Mutation::Add).unwrap(),
            MutationOutcome::Added
        );
        assert_eq!(
            mutate(&state, 20, Mutation::Add).unwrap(),
            MutationOutcome::AlreadyPresent
        );
        assert_eq!(active_judgement_skip_ids(&state).unwrap(), vec![10, 20]);

        let reopened = StateStore::new(&paths).unwrap();
        assert_eq!(active_judgement_skip_ids(&reopened).unwrap(), vec![10, 20]);
        assert_eq!(
            mutate(&reopened, 30, Mutation::Remove).unwrap(),
            MutationOutcome::NotPresent
        );
        assert_eq!(
            mutate(&reopened, 10, Mutation::Remove).unwrap(),
            MutationOutcome::Removed
        );
        assert_eq!(active_judgement_skip_ids(&reopened).unwrap(), vec![20]);
        assert!(contains(&reopened, "20").unwrap());
        assert!(!contains(&reopened, "10").unwrap());
    }

    #[test]
    fn editor_diff_preserves_unrelated_concurrent_changes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        mutate(&state, 10, Mutation::Add).unwrap();
        let original = active_judgement_skip_ids(&state).unwrap();

        mutate(&state, 20, Mutation::Add).unwrap();
        let updated = apply_active_judgement_skip_editor_changes(&state, &original, &[30]).unwrap();

        assert_eq!(updated, vec![20, 30]);
        assert!(apply_active_judgement_skip_editor_changes(&state, &updated, &[0]).is_err());
    }

    #[test]
    fn membership_cache_refreshes_after_peer_store_updates() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let peer = StateStore::new(&paths).unwrap();

        assert!(!contains(&state, "42").unwrap());
        peer.plugin_put_json(&state_scope(), STATE_KEY, &BTreeSet::from([42]))
            .unwrap();
        ids_cache()
            .lock()
            .unwrap()
            .get_mut(state.state_dir())
            .unwrap()
            .checked_at = Instant::now() - CACHE_RECHECK_INTERVAL;
        assert!(contains(&state, "42").unwrap());
        peer.plugin_delete_key(&state_scope(), STATE_KEY).unwrap();
        ids_cache()
            .lock()
            .unwrap()
            .get_mut(state.state_dir())
            .unwrap()
            .checked_at = Instant::now() - CACHE_RECHECK_INTERVAL;
        assert!(!contains(&state, "42").unwrap());
    }

    #[test]
    fn tools_are_visible_only_to_effective_admins_and_are_writes() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());

        let ordinary = test_context(&paths, state.clone(), adapter.clone(), false);
        let mut registry = ToolRegistry::new();
        register_tools(&mut registry, ordinary);
        assert!(registry.get("add_active_judgement_skip_qq").is_none());
        assert!(registry.get("remove_active_judgement_skip_qq").is_none());

        let actor = PlatformAccessActor {
            platform: ONEBOT_PLATFORM.to_string(),
            account_id: "10000".to_string(),
            user_id: "7".to_string(),
            conversation_kind: "private".to_string(),
            conversation_id: "7".to_string(),
            message_id: "grant-message".to_string(),
        };
        state
            .add_platform_access_grant(
                &global_grant_key(AccessPermission::Administrator, "42"),
                &actor,
            )
            .unwrap();
        let dynamic_admin = test_context(&paths, state, adapter, false);
        let mut registry = ToolRegistry::new();
        register_tools(&mut registry, dynamic_admin);
        for name in [
            "add_active_judgement_skip_qq",
            "remove_active_judgement_skip_qq",
        ] {
            let tool = registry.get(name).unwrap();
            assert_eq!(tool.permission, ToolPermission::Writes);
            assert!(tool.always_loaded);
        }
    }

    #[tokio::test]
    async fn tools_mutate_immediately_and_send_one_fixed_output() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state.clone(), adapter.clone(), true);

        let output = run_tool(
            json!({"qq_id": "2477342916"}),
            context.clone(),
            Mutation::Add,
        )
        .await
        .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&output).unwrap()["ok"], true);
        assert_eq!(active_judgement_skip_ids(&state).unwrap(), [2477342916]);
        assert_eq!(adapter.messages.lock().unwrap().len(), 1);
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[0]),
            "已加入跳过主动判断名单：2477342916"
        );
        assert_eq!(context.take_final_reply_suppression_start(20), Some(0));

        run_tool(json!({"qq_id": 2477342916_i64}), context, Mutation::Remove)
            .await
            .unwrap();
        assert!(active_judgement_skip_ids(&state).unwrap().is_empty());
        assert_eq!(adapter.messages.lock().unwrap().len(), 2);
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[1]),
            "已移出跳过主动判断名单：2477342916"
        );
    }

    #[tokio::test]
    async fn handler_rechecks_admin_authority() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state.clone(), adapter.clone(), false);

        let output = run_tool(json!({"qq_id": 99}), context.clone(), Mutation::Add)
            .await
            .unwrap();

        let output = serde_json::from_str::<Value>(&output).unwrap();
        assert_eq!(output["ok"], false);
        assert_eq!(output["operation_succeeded"], false);
        assert!(active_judgement_skip_ids(&state).unwrap().is_empty());
        assert_eq!(
            outbound_text(&adapter.messages.lock().unwrap()[0]),
            "操作失败"
        );
        assert_eq!(context.take_final_reply_suppression_start(20), Some(0));
    }

    #[tokio::test]
    async fn handler_rejects_a_revoked_dynamic_admin() {
        let temp = tempfile::tempdir().unwrap();
        let paths = test_paths(temp.path());
        let state = StateStore::new(&paths).unwrap();
        let actor = PlatformAccessActor {
            platform: ONEBOT_PLATFORM.to_string(),
            account_id: "10000".to_string(),
            user_id: "7".to_string(),
            conversation_kind: "private".to_string(),
            conversation_id: "7".to_string(),
            message_id: "grant-message".to_string(),
        };
        let grant = global_grant_key(AccessPermission::Administrator, "42");
        state.add_platform_access_grant(&grant, &actor).unwrap();
        let adapter = Arc::new(RecordingAdapter::default());
        let context = test_context(&paths, state.clone(), adapter, false);
        assert!(effective_admin(&context));

        state.remove_platform_access_grant(&grant, &actor).unwrap();
        let output = run_tool(json!({"qq_id": 99}), context, Mutation::Add)
            .await
            .unwrap();

        let output = serde_json::from_str::<Value>(&output).unwrap();
        assert_eq!(output["ok"], false);
        assert!(active_judgement_skip_ids(&state).unwrap().is_empty());
    }
}
