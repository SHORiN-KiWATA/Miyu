//! WebUI dashboard 用的好感度档案视图与编辑。
//!
//! 档案键按人格分(`affection_profile:<persona>`),默认人格额外兼容裸键。
//! 编辑走 `plugin_update_json` 读改写,手工改分写一条 source=manual 的事件,
//! 与自动更新并发不互相覆盖。

use super::*;
use crate::config::REAL_CONTEXT_PLUGIN_ID;

const MANUAL_MESSAGE_ID: &str = "dashboard";

pub(crate) fn settings_from_config(config: &AppConfig) -> Result<RealContextPluginSettings> {
    Ok(config
        .platforms
        .qq
        .plugins
        .get(REAL_CONTEXT_PLUGIN_ID)
        .map(RealContextPluginSettings::from_instance)
        .transpose()?
        .unwrap_or_default())
}

fn key_for_persona(persona_scope: &str) -> String {
    format!("{LEGACY_PROFILE_KEY}:{persona_scope}")
}

fn scope_for(account_id: &str, user_id: &str) -> PlatformPluginScopeKey {
    PlatformPluginScopeKey {
        plugin_id: REAL_CONTEXT_PLUGIN_ID.to_string(),
        platform: "onebot".to_string(),
        account_id: account_id.to_string(),
        conversation_kind: "affection".to_string(),
        conversation_id: bounded_text(user_id, 256),
    }
}

fn profile_json(settings: &RealContextPluginSettings, profile: &AffectionProfile) -> Value {
    let level = level_for_score(settings, profile.score, &profile.user_id);
    let mut value = serde_json::to_value(profile).unwrap_or(Value::Null);
    value["level"] = json!(level.name);
    value["reply_bias"] = json!(reply_bias(settings, profile.score, &profile.user_id));
    value["gain_multiplier"] = json!(gain_multiplier(settings, profile.score, &profile.user_id));
    value["max_score"] = json!(max_score_for_user(settings, &profile.user_id));
    value
}

/// 有档案的账号与人格清单(人格从键后缀反推)。
pub(crate) fn dashboard_scopes(store: &StateStore) -> Result<Value> {
    let rows = store.plugin_rows(REAL_CONTEXT_PLUGIN_ID, "affection")?;
    let mut accounts = std::collections::BTreeSet::new();
    let mut personas = std::collections::BTreeSet::new();
    for row in rows {
        accounts.insert(row.scope.account_id);
        if row.key == LEGACY_PROFILE_KEY {
            personas.insert("default".to_string());
        } else if let Some(suffix) = row.key.strip_prefix(&format!("{LEGACY_PROFILE_KEY}:")) {
            personas.insert(suffix.to_string());
        }
    }
    Ok(json!({ "ok": true, "accounts": accounts, "personas": personas }))
}

/// 某账号 + 人格下的全部档案,附等级分布与设置摘要。
pub(crate) fn dashboard_list(
    store: &StateStore,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
) -> Result<Value> {
    let key = key_for_persona(persona_scope);
    let legacy_ok = key == DEFAULT_PROFILE_KEY;
    let rows = store.plugin_rows(REAL_CONTEXT_PLUGIN_ID, "affection")?;
    let mut by_user: std::collections::BTreeMap<String, AffectionProfile> =
        std::collections::BTreeMap::new();
    for row in rows {
        if row.scope.account_id != account_id {
            continue;
        }
        let is_scoped = row.key == key;
        let is_legacy = legacy_ok && row.key == LEGACY_PROFILE_KEY;
        if !is_scoped && !is_legacy {
            continue;
        }
        let Ok(mut profile) = serde_json::from_str::<AffectionProfile>(&row.value_json) else {
            continue;
        };
        normalize_profile(&mut profile, settings, &row.scope.conversation_id);
        // 分键与裸键同时存在时以分键为准(load_profile 的语义)。
        if is_legacy && by_user.contains_key(&row.scope.conversation_id) {
            continue;
        }
        by_user.insert(row.scope.conversation_id, profile);
    }
    let mut levels: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut today_gain = 0.0;
    let mut today_loss = 0.0;
    let mut auto_off = 0usize;
    let items: Vec<Value> = by_user
        .values()
        .map(|profile| {
            let level = level_for_score(settings, profile.score, &profile.user_id);
            *levels.entry(level.name).or_insert(0) += 1;
            if profile.daily_date == today {
                today_gain += profile.daily_gain;
                today_loss += profile.daily_loss;
            }
            if !profile.auto_update_enabled {
                auto_off += 1;
            }
            let mut value = profile_json(settings, profile);
            // 列表不带事件明细,抽屉再取。
            value["event_count"] = json!(profile.events.len());
            value["events"] = Value::Null;
            value
        })
        .collect();
    Ok(json!({
        "ok": true,
        "account_id": account_id,
        "persona": persona_scope,
        "enabled": settings.affection_enable,
        "update_enabled": settings.affection_update_enable,
        "items": items,
        "levels": levels,
        "today": today,
        "today_gain": today_gain,
        "today_loss": today_loss,
        "auto_update_off": auto_off,
        "limits": {
            "initial": settings.affection_initial_score,
            "min": settings.affection_min_score,
            "max": settings.affection_max_score,
            "regular_max": settings.affection_regular_max_score,
            "daily_gain": settings.affection_daily_gain_limit,
            "daily_loss": settings.affection_daily_loss_limit,
            "max_tags": settings.affection_max_tags,
        },
    }))
}

pub(crate) fn dashboard_profile(
    store: &StateStore,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
    user_id: &str,
) -> Result<Option<Value>> {
    let scope = scope_for(account_id, user_id);
    let key = key_for_persona(persona_scope);
    let Some(mut profile) = load_profile(store, &scope, &key)? else {
        return Ok(None);
    };
    normalize_profile(&mut profile, settings, user_id);
    Ok(Some(profile_json(settings, &profile)))
}

#[derive(Default)]
pub(crate) struct DashboardPatch {
    pub(crate) score: Option<f64>,
    pub(crate) note: Option<String>,
    pub(crate) tags: Option<Vec<String>>,
    pub(crate) auto_update_enabled: Option<bool>,
    pub(crate) clear_events: bool,
    pub(crate) reason: String,
}

pub(crate) fn dashboard_update(
    store: &StateStore,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
    user_id: &str,
    patch: DashboardPatch,
) -> Result<Option<Value>> {
    let scope = scope_for(account_id, user_id);
    let key = key_for_persona(persona_scope);
    if load_profile(store, &scope, &key)?.is_none() {
        return Ok(None);
    }
    let now = now_unix();
    let reason = bounded_single_line(
        if patch.reason.trim().is_empty() {
            "dashboard 手动调整"
        } else {
            patch.reason.trim()
        },
        MAX_REASON_CHARS,
    );
    let updated = store.plugin_update_json(&scope, &key, |current: Option<AffectionProfile>| {
        let mut profile = match current {
            Some(profile) => profile,
            // 只有裸键(默认人格旧档案)时,在分键下写一份新的。
            None => match store.plugin_get_json::<AffectionProfile>(&scope, LEGACY_PROFILE_KEY)? {
                Some(profile) if key == DEFAULT_PROFILE_KEY => profile,
                _ => return Ok(None),
            },
        };
        normalize_profile(&mut profile, settings, user_id);
        if patch.clear_events {
            profile.events.clear();
        }
        if let Some(score) = patch.score {
            let before = profile.score;
            let after = clamp_score(settings, score, user_id);
            if (after - before).abs() > 0.0001 {
                profile.score = after;
                profile.events.insert(
                    0,
                    AffectionEvent {
                        delta: after - before,
                        score_before: before,
                        score_after: after,
                        confidence: 1.0,
                        reason: reason.clone(),
                        tags_add: Vec::new(),
                        tags_remove: Vec::new(),
                        message_id: MANUAL_MESSAGE_ID.to_string(),
                        created_at: now,
                    },
                );
                profile.events.truncate(MAX_STORED_EVENTS);
            }
        }
        if let Some(note) = patch.note.as_deref() {
            profile.note = bounded_text(note, MAX_NOTE_CHARS);
        }
        if let Some(tags) = patch.tags.clone() {
            profile.tags = clean_tags(tags, settings.affection_max_tags);
        }
        if let Some(enabled) = patch.auto_update_enabled {
            profile.auto_update_enabled = enabled;
        }
        profile.updated_at = now;
        Ok(Some(profile))
    })?;
    Ok(updated.map(|profile| profile_json(settings, &profile)))
}

pub(crate) fn dashboard_delete(
    store: &StateStore,
    account_id: &str,
    persona_scope: &str,
    user_id: &str,
) -> Result<bool> {
    let scope = scope_for(account_id, user_id);
    let key = key_for_persona(persona_scope);
    let mut deleted = store.plugin_delete_key(&scope, &key)?;
    if key == DEFAULT_PROFILE_KEY {
        deleted |= store.plugin_delete_key(&scope, LEGACY_PROFILE_KEY)?;
    }
    Ok(deleted)
}
