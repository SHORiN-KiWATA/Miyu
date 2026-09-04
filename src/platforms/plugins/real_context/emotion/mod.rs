//! 情绪状态(09-04)。
//!
//! 按 (bot 账号, 人格) 一份的二维状态:valence 心情 −1..1(基线 0)、arousal
//! 表达欲 0..1(基线 0.5)。存 `platform_plugin_kv`,与好感度同表同键法。
//!
//! 衰减在读的时候惰性算,不起定时器;只有写入(回复后 touch、LLM 增量、手动)
//! 才把衰减后的值落盘,纯读不写。"有效状态"再叠加时段与冷清修正,不落盘。
//!
//! 影响两处:判官阈值修正(与好感/热度并列的一项程序调整)、回合尾部的
//! `<internal-state>` 一行陈述(只在偏离基线时给,平静时不说话——回合尾部不在
//! 缓存前缀里,但也不必往化石里塞废话)。

use crate::config::{AppConfig, RealContextPluginSettings, REAL_CONTEXT_PLUGIN_ID};
use crate::paths::MiyuPaths;
use crate::platforms::PlatformTurnContext;
use crate::state::{PlatformPluginScopeKey, StateStore};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub(crate) const MAX_EVENTS: usize = 100;
const STATE_KEY_PREFIX: &str = "emotion_state";
const VALENCE_BASELINE: f64 = 0.0;
const AROUSAL_BASELINE: f64 = 0.5;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct EmotionState {
    pub(crate) version: u32,
    pub(crate) valence: f64,
    pub(crate) arousal: f64,
    pub(crate) updated_at: i64,
    pub(crate) daily_date: String,
    pub(crate) daily_gain: f64,
    pub(crate) daily_loss: f64,
    pub(crate) daily_interactions: u64,
    pub(crate) last_interaction_at: i64,
    pub(crate) events: Vec<EmotionEvent>,
}

impl Default for EmotionState {
    fn default() -> Self {
        Self {
            version: 1,
            valence: VALENCE_BASELINE,
            arousal: AROUSAL_BASELINE,
            updated_at: 0,
            daily_date: String::new(),
            daily_gain: 0.0,
            daily_loss: 0.0,
            daily_interactions: 0,
            last_interaction_at: 0,
            events: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct EmotionEvent {
    pub(crate) delta_valence: f64,
    pub(crate) delta_arousal: f64,
    pub(crate) valence_before: f64,
    pub(crate) valence_after: f64,
    pub(crate) arousal_before: f64,
    pub(crate) arousal_after: f64,
    pub(crate) label_before: String,
    pub(crate) label_after: String,
    /// reply | llm | moderation | manual
    pub(crate) source: String,
    pub(crate) reason: String,
    pub(crate) group_id: String,
    pub(crate) message_id: String,
    pub(crate) created_at: i64,
}

impl Default for EmotionEvent {
    fn default() -> Self {
        Self {
            delta_valence: 0.0,
            delta_arousal: 0.0,
            valence_before: VALENCE_BASELINE,
            valence_after: VALENCE_BASELINE,
            arousal_before: AROUSAL_BASELINE,
            arousal_after: AROUSAL_BASELINE,
            label_before: String::new(),
            label_after: String::new(),
            source: String::new(),
            reason: String::new(),
            group_id: String::new(),
            message_id: String::new(),
            created_at: 0,
        }
    }
}

/* ── 纯函数:标签、文本、衰减、修正 ─────────────────────────── */

pub(crate) fn clamp_valence(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        VALENCE_BASELINE
    }
}

pub(crate) fn clamp_arousal(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        AROUSAL_BASELINE
    }
}

/// 七态标签,顺序判定;阈值是常量不是配置。
pub(crate) fn label_for(valence: f64, arousal: f64) -> &'static str {
    let v = clamp_valence(valence);
    let a = clamp_arousal(arousal);
    if v <= -0.35 && a >= 0.45 {
        "烦躁"
    } else if v <= -0.35 {
        "低落"
    } else if a <= 0.25 {
        "疲惫"
    } else if v >= 0.35 && a >= 0.70 {
        "兴奋"
    } else if v >= 0.25 && a >= 0.65 {
        "调皮"
    } else if v >= 0.30 {
        "愉快"
    } else {
        "平静"
    }
}

pub(crate) fn label_en(label: &str) -> &'static str {
    match label {
        "烦躁" => "irritable",
        "低落" => "down",
        "疲惫" => "tired",
        "兴奋" => "excited",
        "调皮" => "playful",
        "愉快" => "cheerful",
        _ => "calm",
    }
}

pub(crate) fn valence_text(value: f64) -> &'static str {
    if value >= 0.55 {
        "很好"
    } else if value >= 0.18 {
        "不错"
    } else if value <= -0.55 {
        "很差"
    } else if value <= -0.18 {
        "偏低"
    } else {
        "平稳"
    }
}

pub(crate) fn arousal_text(value: f64) -> &'static str {
    if value >= 0.75 {
        "很有表达欲"
    } else if value >= 0.55 {
        "比较有精神"
    } else if value <= 0.25 {
        "很没精神"
    } else if value <= 0.45 {
        "有点犯困"
    } else {
        "普通"
    }
}

/// 指数回归基线:半衰期 h,经过 Δt 后偏离量减半。
pub(crate) fn decayed(
    state: &EmotionState,
    now: i64,
    settings: &RealContextPluginSettings,
) -> (f64, f64) {
    let elapsed = (now - state.updated_at).max(0) as f64;
    if state.updated_at <= 0 || elapsed <= 0.0 {
        return (clamp_valence(state.valence), clamp_arousal(state.arousal));
    }
    let v_half = (settings.emotion_valence_half_life_hours * 3600.0).max(60.0);
    let a_half = (settings.emotion_arousal_half_life_minutes * 60.0).max(60.0);
    let v = VALENCE_BASELINE
        + (clamp_valence(state.valence) - VALENCE_BASELINE) * (2f64).powf(-elapsed / v_half);
    let a = AROUSAL_BASELINE
        + (clamp_arousal(state.arousal) - AROUSAL_BASELINE) * (2f64).powf(-elapsed / a_half);
    (clamp_valence(v), clamp_arousal(a))
}

/// 心情好、表达欲高 → 阈值降(负数);反之升。
pub(crate) fn threshold_adjust(
    valence: f64,
    arousal: f64,
    settings: &RealContextPluginSettings,
) -> f64 {
    if !settings.emotion_influence_threshold || settings.emotion_max_threshold_adjust <= 0.0 {
        return 0.0;
    }
    let factor = (clamp_valence(valence) + (2.0 * clamp_arousal(arousal) - 1.0)) / 2.0;
    let max = settings.emotion_max_threshold_adjust;
    (-factor * max).clamp(-max, max)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Effective {
    pub(crate) stored_valence: f64,
    pub(crate) stored_arousal: f64,
    pub(crate) valence: f64,
    pub(crate) arousal: f64,
    pub(crate) label: &'static str,
    pub(crate) stored_label: &'static str,
    pub(crate) time_arousal_adjust: f64,
    pub(crate) idle_hours: f64,
    pub(crate) idle_valence_adjust: f64,
    pub(crate) idle_arousal_adjust: f64,
    pub(crate) threshold_adjust: f64,
}

/// 存储态(已衰减)叠加时段与冷清修正 = 有效态。`hour` 是本地小时,
/// `idle_secs` 是距最近一条人类消息的秒数(None = 不知道,不修正)。
pub(crate) fn effective(
    state: &EmotionState,
    now: i64,
    hour: u32,
    idle_secs: Option<i64>,
    settings: &RealContextPluginSettings,
) -> Effective {
    let (sv, sa) = decayed(state, now, settings);
    let time_adjust = if (6..10).contains(&hour) {
        settings.emotion_morning_arousal_bonus
    } else if hour >= 23 || hour < 6 {
        -settings.emotion_night_arousal_penalty
    } else {
        0.0
    };
    let mut idle_hours = 0.0;
    let mut idle_v = 0.0;
    let mut idle_a = 0.0;
    if let Some(secs) = idle_secs {
        idle_hours = (secs.max(0) as f64) / 3600.0;
        let threshold = settings.emotion_idle_loneliness_hours.max(0.1);
        if idle_hours > threshold {
            let pressure = ((idle_hours - threshold) / (threshold * 2.0).max(1.0)).min(1.0);
            idle_v = -0.18 * pressure;
            idle_a = -0.10 * pressure;
        }
    }
    let v = clamp_valence(sv + idle_v);
    let a = clamp_arousal(sa + time_adjust + idle_a);
    Effective {
        stored_valence: sv,
        stored_arousal: sa,
        valence: v,
        arousal: a,
        label: label_for(v, a),
        stored_label: label_for(sv, sa),
        time_arousal_adjust: time_adjust,
        idle_hours,
        idle_valence_adjust: idle_v,
        idle_arousal_adjust: idle_a,
        threshold_adjust: threshold_adjust(v, a, settings),
    }
}

/// 回合尾部那一行:偏离基线才说;只陈述是什么,不写数值不下指令。
pub(crate) fn tone_hint(effective: &Effective) -> Option<String> {
    let mood = valence_text(effective.valence);
    let energy = arousal_text(effective.arousal);
    if mood == "平稳" && energy == "普通" {
        return None;
    }
    Some(format!(
        "<internal-state>心情:{mood};精神:{energy}。</internal-state>"
    ))
}

/* ── 存储 ───────────────────────────────────────────────── */

pub(crate) fn scope_for(account_id: &str) -> PlatformPluginScopeKey {
    PlatformPluginScopeKey {
        plugin_id: REAL_CONTEXT_PLUGIN_ID.to_string(),
        platform: "onebot".to_string(),
        account_id: account_id.to_string(),
        conversation_kind: "emotion".to_string(),
        conversation_id: "*".to_string(),
    }
}

pub(crate) fn key_for(persona_scope: &str) -> String {
    format!("{STATE_KEY_PREFIX}:{persona_scope}")
}

pub(crate) fn load(
    store: &StateStore,
    account_id: &str,
    persona_scope: &str,
) -> Result<EmotionState> {
    Ok(store
        .plugin_get_json::<EmotionState>(&scope_for(account_id), &key_for(persona_scope))?
        .unwrap_or_default())
}

pub(crate) struct Delta<'a> {
    pub(crate) valence: f64,
    pub(crate) arousal: f64,
    pub(crate) source: &'a str,
    pub(crate) reason: &'a str,
    pub(crate) group_id: &'a str,
    pub(crate) message_id: &'a str,
    /// 计入今日互动次数(回复类事件为 true,手动/LLM 修正为 false)。
    pub(crate) interaction: bool,
}

/// 读改写:先把存储态衰减到现在,再加增量、套日限幅、落盘并记事件。
pub(crate) fn apply_delta(
    store: &StateStore,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
    now: i64,
    delta: Delta<'_>,
) -> Result<EmotionState> {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let updated = store.plugin_update_json(
        &scope_for(account_id),
        &key_for(persona_scope),
        |current: Option<EmotionState>| {
            let mut state = current.unwrap_or_default();
            let (v0, a0) = decayed(&state, now, settings);
            if state.daily_date != today {
                state.daily_date = today.clone();
                state.daily_gain = 0.0;
                state.daily_loss = 0.0;
                state.daily_interactions = 0;
            }
            let mut dv = if delta.valence.is_finite() {
                delta.valence
            } else {
                0.0
            };
            let da = if delta.arousal.is_finite() {
                delta.arousal
            } else {
                0.0
            };
            if dv > 0.0 && settings.emotion_daily_valence_gain_limit > 0.0 {
                dv =
                    dv.min((settings.emotion_daily_valence_gain_limit - state.daily_gain).max(0.0));
            } else if dv < 0.0 && settings.emotion_daily_valence_loss_limit > 0.0 {
                dv = -(-dv)
                    .min((settings.emotion_daily_valence_loss_limit - state.daily_loss).max(0.0));
            }
            let v1 = clamp_valence(v0 + dv);
            let a1 = clamp_arousal(a0 + da);
            let actual_dv = v1 - v0;
            if actual_dv > 0.0 {
                state.daily_gain += actual_dv;
            } else if actual_dv < 0.0 {
                state.daily_loss += -actual_dv;
            }
            if delta.interaction {
                state.daily_interactions += 1;
                state.last_interaction_at = now;
            }
            if actual_dv.abs() >= 0.001 || (a1 - a0).abs() >= 0.001 {
                state.events.insert(
                    0,
                    EmotionEvent {
                        delta_valence: actual_dv,
                        delta_arousal: a1 - a0,
                        valence_before: v0,
                        valence_after: v1,
                        arousal_before: a0,
                        arousal_after: a1,
                        label_before: label_for(v0, a0).to_string(),
                        label_after: label_for(v1, a1).to_string(),
                        source: delta.source.to_string(),
                        reason: delta.reason.chars().take(300).collect(),
                        group_id: delta.group_id.chars().take(64).collect(),
                        message_id: delta.message_id.chars().take(64).collect(),
                        created_at: now,
                    },
                );
                state.events.truncate(MAX_EVENTS);
            }
            state.version = 1;
            state.valence = v1;
            state.arousal = a1;
            state.updated_at = now;
            Ok(Some(state))
        },
    )?;
    Ok(updated.unwrap_or_default())
}

/* ── 回合接入 ───────────────────────────────────────────── */

pub(crate) struct ReplyFacts {
    pub(crate) direct: bool,
    pub(crate) active: bool,
    pub(crate) moderation_hit: bool,
    pub(crate) reply_chars: usize,
}

/// 层①启发式增量表(设计稿 §2)。
pub(crate) fn heuristic_delta(facts: &ReplyFacts) -> (f64, f64, &'static str) {
    let mut dv = 0.02_f64;
    let mut da = 0.02_f64;
    let mut reason = "完成了一次普通回复";
    if facts.direct {
        dv += 0.02;
        da += 0.015;
        reason = "完成了一次直接互动";
    }
    if facts.active {
        dv += 0.015;
        da += 0.02;
        reason = "主动接上了群聊话题";
    }
    if facts.moderation_hit {
        dv = (dv - 0.055).min(-0.025);
        da += 0.02;
        reason = "处理了可能违规或令人不适的内容";
    }
    if facts.reply_chars >= 120 {
        da += 0.01;
    }
    (dv, da, reason)
}

fn persona_scope(config: &AppConfig) -> String {
    config.active_persona_scope()
}

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 回复发出后:`skip_heuristic` 为 true 时(层②会跟着好感度更新一起来)只计
/// 互动不加分。
pub(crate) fn touch_after_reply(
    context: &PlatformTurnContext,
    settings: &RealContextPluginSettings,
    facts: &ReplyFacts,
    skip_heuristic: bool,
) -> Result<()> {
    if !settings.emotion_enable {
        return Ok(());
    }
    let (dv, da, reason) = if settings.emotion_heuristic_enable && !skip_heuristic {
        heuristic_delta(facts)
    } else {
        (0.0, 0.0, "")
    };
    let message_id = context
        .inbound_event()
        .map(|event| event.message_id.clone())
        .unwrap_or_default();
    apply_delta(
        &context.state_store,
        settings,
        &context.conversation.account_id,
        &persona_scope(&context.config),
        now_unix(),
        Delta {
            valence: dv,
            arousal: da,
            source: if facts.moderation_hit {
                "moderation"
            } else {
                "reply"
            },
            reason,
            group_id: &context.conversation.conversation_id,
            message_id: &message_id,
            interaction: true,
        },
    )?;
    Ok(())
}

/// 层②:好感度更新那次 LLM 调用带回来的语义增量,范围各 [−0.15, +0.10]。
pub(crate) fn apply_llm_delta(
    store: &StateStore,
    config: &AppConfig,
    settings: &RealContextPluginSettings,
    account_id: &str,
    group_id: &str,
    message_id: &str,
    valence_delta: f64,
    arousal_delta: f64,
    reason: &str,
) -> Result<()> {
    if !settings.emotion_enable || !settings.emotion_llm_enrich_enable {
        return Ok(());
    }
    apply_delta(
        store,
        settings,
        account_id,
        &persona_scope(config),
        now_unix(),
        Delta {
            valence: valence_delta.clamp(-0.15, 0.10),
            arousal: arousal_delta.clamp(-0.15, 0.10),
            source: "llm",
            reason,
            group_id,
            message_id,
            interaction: false,
        },
    )?;
    Ok(())
}

pub(crate) struct Snapshot {
    pub(crate) effective: Effective,
    pub(crate) tone_hint: Option<String>,
}

fn local_hour() -> u32 {
    use chrono::Timelike;
    chrono::Local::now().hour()
}

/// 判官与注入用:读存储态 + 最近人类消息时间 → 有效态。功能关着返回 None。
pub(crate) fn snapshot(
    context: &PlatformTurnContext,
    settings: &RealContextPluginSettings,
) -> Result<Option<Snapshot>> {
    if !settings.emotion_enable {
        return Ok(None);
    }
    Ok(Some(snapshot_for(
        &context.state_store,
        &context.paths,
        settings,
        &context.conversation.account_id,
        &persona_scope(&context.config),
    )?))
}

pub(crate) fn snapshot_for(
    store: &StateStore,
    paths: &MiyuPaths,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
) -> Result<Snapshot> {
    let state = load(store, account_id, persona_scope)?;
    let now = now_unix();
    let idle = crate::platforms::plugins::message_history::dashboard::latest_human_message_at(
        paths, account_id,
    )
    .ok()
    .flatten()
    .map(|at| now - at);
    let effective = effective(&state, now, local_hour(), idle, settings);
    let tone_hint = if settings.emotion_influence_tone {
        tone_hint(&effective)
    } else {
        None
    };
    Ok(Snapshot {
        effective,
        tone_hint,
    })
}

/* ── dashboard ─────────────────────────────────────────── */

pub(crate) fn dashboard_state(
    store: &StateStore,
    paths: &MiyuPaths,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
) -> Result<Value> {
    let state = load(store, account_id, persona_scope)?;
    let snapshot = snapshot_for(store, paths, settings, account_id, persona_scope)?;
    let e = &snapshot.effective;
    let now = now_unix();
    // 距基线回归到 5% 以内还要多久(按半衰期算,给面板一个"多久归零")。
    let remaining = |value: f64, baseline: f64, half_life_secs: f64| -> f64 {
        let offset = (value - baseline).abs();
        if offset < 0.01 {
            0.0
        } else {
            half_life_secs * (offset / 0.01).log2()
        }
    };
    Ok(json!({
        "ok": true,
        "enabled": settings.emotion_enable,
        "account_id": account_id,
        "persona": persona_scope,
        "stored": {
            "valence": e.stored_valence,
            "arousal": e.stored_arousal,
            "label": e.stored_label,
            "label_en": label_en(e.stored_label),
            "updated_at": state.updated_at,
            "raw_valence": state.valence,
            "raw_arousal": state.arousal,
        },
        "effective": {
            "valence": e.valence,
            "arousal": e.arousal,
            "label": e.label,
            "label_en": label_en(e.label),
            "valence_text": valence_text(e.valence),
            "arousal_text": arousal_text(e.arousal),
            "threshold_adjust": e.threshold_adjust,
            "time_arousal_adjust": e.time_arousal_adjust,
            "idle_hours": e.idle_hours,
            "idle_valence_adjust": e.idle_valence_adjust,
            "idle_arousal_adjust": e.idle_arousal_adjust,
            "tone_hint": snapshot.tone_hint,
        },
        "return_secs": {
            "valence": remaining(e.stored_valence, VALENCE_BASELINE, settings.emotion_valence_half_life_hours * 3600.0),
            "arousal": remaining(e.stored_arousal, AROUSAL_BASELINE, settings.emotion_arousal_half_life_minutes * 60.0),
        },
        "daily": {
            "date": state.daily_date,
            "gain": state.daily_gain,
            "loss": state.daily_loss,
            "interactions": state.daily_interactions,
            "gain_limit": settings.emotion_daily_valence_gain_limit,
            "loss_limit": settings.emotion_daily_valence_loss_limit,
        },
        "last_interaction_at": state.last_interaction_at,
        "now": now,
        "events": state.events,
        "settings": {
            "heuristic": settings.emotion_heuristic_enable,
            "llm_enrich": settings.emotion_llm_enrich_enable,
            "influence_threshold": settings.emotion_influence_threshold,
            "max_threshold_adjust": settings.emotion_max_threshold_adjust,
            "influence_tone": settings.emotion_influence_tone,
            "valence_half_life_hours": settings.emotion_valence_half_life_hours,
            "arousal_half_life_minutes": settings.emotion_arousal_half_life_minutes,
            "idle_loneliness_hours": settings.emotion_idle_loneliness_hours,
        },
    }))
}

/// 手动设值:写成一条 manual 事件(增量 = 目标 − 当前衰减态),不套日限幅。
pub(crate) fn dashboard_set(
    store: &StateStore,
    settings: &RealContextPluginSettings,
    account_id: &str,
    persona_scope: &str,
    valence: f64,
    arousal: f64,
    reason: &str,
) -> Result<EmotionState> {
    let now = now_unix();
    let current = load(store, account_id, persona_scope)?;
    let (v0, a0) = decayed(&current, now, settings);
    let mut relaxed = settings.clone();
    relaxed.emotion_daily_valence_gain_limit = 0.0;
    relaxed.emotion_daily_valence_loss_limit = 0.0;
    apply_delta(
        store,
        &relaxed,
        account_id,
        persona_scope,
        now,
        Delta {
            valence: clamp_valence(valence) - v0,
            arousal: clamp_arousal(arousal) - a0,
            source: "manual",
            reason: if reason.trim().is_empty() {
                "dashboard 手动设值"
            } else {
                reason.trim()
            },
            group_id: "",
            message_id: "dashboard",
            interaction: false,
        },
    )
}

pub(crate) fn dashboard_reset(
    store: &StateStore,
    account_id: &str,
    persona_scope: &str,
    clear_events: bool,
) -> Result<()> {
    if clear_events {
        store.plugin_delete_key(&scope_for(account_id), &key_for(persona_scope))?;
        return Ok(());
    }
    store.plugin_update_json(
        &scope_for(account_id),
        &key_for(persona_scope),
        |current: Option<EmotionState>| {
            let mut state = current.unwrap_or_default();
            state.valence = VALENCE_BASELINE;
            state.arousal = AROUSAL_BASELINE;
            state.updated_at = now_unix();
            Ok(Some(state))
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> RealContextPluginSettings {
        RealContextPluginSettings {
            emotion_enable: true,
            ..Default::default()
        }
    }

    #[test]
    fn labels_follow_the_table_in_order() {
        assert_eq!(label_for(-0.5, 0.6), "烦躁");
        assert_eq!(label_for(-0.5, 0.2), "低落");
        assert_eq!(label_for(0.0, 0.2), "疲惫");
        assert_eq!(label_for(0.5, 0.8), "兴奋");
        assert_eq!(label_for(0.28, 0.68), "调皮");
        assert_eq!(label_for(0.4, 0.5), "愉快");
        assert_eq!(label_for(0.0, 0.5), "平静");
        assert_eq!(label_for(f64::NAN, f64::NAN), "平静");
    }

    #[test]
    fn decay_halves_the_offset_per_half_life_and_never_overshoots() {
        let s = settings();
        let state = EmotionState {
            valence: 0.8,
            arousal: 0.9,
            updated_at: 1_000,
            ..Default::default()
        };
        let (v, a) = decayed(&state, 1_000, &s);
        assert!((v - 0.8).abs() < 1e-9 && (a - 0.9).abs() < 1e-9);
        let (v, a) = decayed(&state, 1_000 + 6 * 3600, &s);
        assert!((v - 0.4).abs() < 1e-6);
        assert!((a - (0.5 + 0.4 * (2f64).powf(-8.0))).abs() < 1e-6);
        let (v, a) = decayed(&state, 1_000 + 365 * 86_400, &s);
        assert!(v.abs() < 1e-6 && (a - 0.5).abs() < 1e-6);
        // updated_at 为 0(从未写过)按不衰减处理。
        let fresh = EmotionState {
            valence: 0.3,
            ..Default::default()
        };
        assert_eq!(decayed(&fresh, 99_999, &s).0, 0.3);
    }

    #[test]
    fn threshold_adjust_is_signed_and_bounded() {
        let s = settings();
        assert!(threshold_adjust(1.0, 1.0, &s) < 0.0);
        assert!((threshold_adjust(1.0, 1.0, &s) + 0.12).abs() < 1e-9);
        assert!((threshold_adjust(-1.0, 0.0, &s) - 0.12).abs() < 1e-9);
        assert_eq!(threshold_adjust(0.0, 0.5, &s), 0.0);
        let mut off = settings();
        off.emotion_influence_threshold = false;
        assert_eq!(threshold_adjust(1.0, 1.0, &off), 0.0);
    }

    #[test]
    fn effective_state_stacks_time_and_loneliness_without_touching_storage() {
        let s = settings();
        let state = EmotionState {
            valence: 0.1,
            arousal: 0.5,
            updated_at: 10,
            ..Default::default()
        };
        let quiet = effective(&state, 10, 12, Some(0), &s);
        assert_eq!(quiet.label, "平静");
        assert!(tone_hint(&quiet).is_none());
        let lonely = effective(&state, 10, 12, Some(9 * 3600), &s);
        assert!(lonely.valence < quiet.valence);
        assert!(lonely.idle_valence_adjust < 0.0);
        let night = effective(&state, 10, 2, Some(0), &s);
        assert!((night.arousal - (0.5 - 0.12)).abs() < 1e-9);
        assert_eq!(night.stored_arousal, 0.5);
        let morning = effective(&state, 10, 8, Some(0), &s);
        assert!((morning.arousal - 0.56).abs() < 1e-9);
        let excited = EmotionState {
            valence: 0.6,
            arousal: 0.8,
            updated_at: 10,
            ..Default::default()
        };
        let hint = tone_hint(&effective(&excited, 10, 12, None, &s)).unwrap();
        assert!(hint.contains("心情:很好") && hint.contains("很有表达欲"));
        assert!(!hint.contains('0'));
    }

    #[test]
    fn heuristic_table_matches_the_design() {
        let plain = heuristic_delta(&ReplyFacts {
            direct: false,
            active: false,
            moderation_hit: false,
            reply_chars: 10,
        });
        assert_eq!(plain, (0.02, 0.02, "完成了一次普通回复"));
        let direct = heuristic_delta(&ReplyFacts {
            direct: true,
            active: false,
            moderation_hit: false,
            reply_chars: 200,
        });
        assert!((direct.0 - 0.04).abs() < 1e-9 && (direct.1 - 0.045).abs() < 1e-9);
        let bad = heuristic_delta(&ReplyFacts {
            direct: true,
            active: true,
            moderation_hit: true,
            reply_chars: 0,
        });
        assert!(bad.0 <= -0.025);
        assert_eq!(bad.2, "处理了可能违规或令人不适的内容");
    }

    #[test]
    fn apply_delta_respects_daily_limits_and_records_events() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let paths = MiyuPaths {
            root_dir: root.to_path_buf(),
            config_dir: root.join("config"),
            config_file: root.join("config/config.jsonc"),
            skills_dir: root.join("config/skills"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            pictures_dir: root.join("pictures"),
            fish_hook_file: root.join("fish"),
            bash_hook_file: root.join("bash"),
            zsh_hook_file: root.join("zsh"),
            scripts_dir: root.join("scripts"),
            system_scripts_dir: root.join("system-scripts"),
        };
        let store = StateStore::new(&paths).unwrap();
        let s = settings();
        let first = apply_delta(
            &store,
            &s,
            "1",
            "default",
            1_000,
            Delta {
                valence: 0.5,
                arousal: 0.1,
                source: "reply",
                reason: "r",
                group_id: "g",
                message_id: "m",
                interaction: true,
            },
        )
        .unwrap();
        assert!((first.valence - 0.5).abs() < 1e-9);
        assert_eq!(first.daily_interactions, 1);
        assert_eq!(first.events.len(), 1);
        assert_eq!(first.events[0].label_after, "愉快");
        // 日增益上限 0.6:再加 0.5 只能进 0.1。
        let second = apply_delta(
            &store,
            &s,
            "1",
            "default",
            1_000,
            Delta {
                valence: 0.5,
                arousal: 0.0,
                source: "reply",
                reason: "r",
                group_id: "g",
                message_id: "m2",
                interaction: true,
            },
        )
        .unwrap();
        assert!((second.valence - 0.6).abs() < 1e-9);
        assert!((second.daily_gain - 0.6).abs() < 1e-9);
        // 手动设值绕过日限幅并落一条 manual 事件。
        let set = dashboard_set(&store, &s, "1", "default", -0.9, 0.2, "").unwrap();
        assert!((set.valence + 0.9).abs() < 1e-9);
        assert_eq!(set.events[0].source, "manual");
        assert_eq!(load(&store, "1", "default").unwrap().events.len(), 3);
        dashboard_reset(&store, "1", "default", false).unwrap();
        let reset = load(&store, "1", "default").unwrap();
        assert_eq!(reset.valence, 0.0);
        assert_eq!(reset.events.len(), 3);
        dashboard_reset(&store, "1", "default", true).unwrap();
        assert!(load(&store, "1", "default").unwrap().events.is_empty());
    }
}
