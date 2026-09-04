//! WebUI dashboard 用的群管记录视图:统一事件流(带派生禁言状态)、成员汇总、
//! 两份旧记录原样、bot 在群身份;以及清空事件流。

use super::records::*;
use crate::state::{PlatformPluginScopeKey, StateStore};
use anyhow::Result;
use serde_json::{json, Value};

pub(crate) fn dashboard_management(
    store: &StateStore,
    scope: &PlatformPluginScopeKey,
) -> Result<Value> {
    let events = load_all_events_from(store, scope)?;
    let statuses = ban_statuses(&events, chrono::Utc::now().timestamp());
    let members = aggregate_member_stats("all", &events);
    let offenders = store
        .plugin_get_json::<Value>(scope, OFFENDERS_KEY)?
        .unwrap_or_else(|| json!({}));
    let kicks = store
        .plugin_get_json::<Value>(scope, KICKS_KEY)?
        .unwrap_or_else(|| json!([]));
    let bot_role = store
        .plugin_get_json::<String>(scope, ROLE_KEY)?
        .unwrap_or_default();
    let events_json: Vec<Value> = events
        .iter()
        .map(|event| {
            let mut value = serde_json::to_value(event).unwrap_or(Value::Null);
            if let Some(status) = statuses.get(&event.record_id) {
                value["status"] = json!(status);
            }
            value
        })
        .collect();
    Ok(json!({
        "ok": true,
        "bot_role": bot_role,
        "events": events_json,
        "members": members,
        "offenders": offenders,
        "kicks": kicks,
    }))
}

pub(crate) fn dashboard_clear_events(
    store: &StateStore,
    scope: &PlatformPluginScopeKey,
) -> Result<bool> {
    store.plugin_delete_key(scope, EVENTS_KEY)
}
