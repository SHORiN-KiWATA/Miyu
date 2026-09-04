//! 处置记录与历史查询。
//!
//! 禁言、踢出、警告都要留痕：`OffenderHistory` 累积同一个人的历史，让「屡犯」
//! 可判定。记录只增不改——改历史等于毁掉判断依据。

use crate::platforms::plugins::group_management::*;
use crate::platforms::plugins::real_context::safe_prompt_field;

pub(in crate::platforms::plugins::group_management) const ROLE_KEY: &str =
    "qq_group_management.bot_role";

pub(in crate::platforms::plugins::group_management) const OFFENDERS_KEY: &str = "offender_history";

pub(in crate::platforms::plugins::group_management) const KICKS_KEY: &str = "kick_history";

pub(in crate::platforms::plugins::group_management) const EVENTS_KEY: &str = "management_events";

#[derive(Clone, Debug, Serialize)]
pub(in crate::platforms::plugins::group_management) struct BanRecord {
    pub(in crate::platforms::plugins::group_management) record_id: String,
    pub(in crate::platforms::plugins::group_management) group_id: String,
    pub(in crate::platforms::plugins::group_management) user_id: String,
    pub(in crate::platforms::plugins::group_management) user_name: String,
    pub(in crate::platforms::plugins::group_management) duration: u64,
    pub(in crate::platforms::plugins::group_management) started_at: i64,
    pub(in crate::platforms::plugins::group_management) expires_at: i64,
    pub(in crate::platforms::plugins::group_management) status: String,
    pub(in crate::platforms::plugins::group_management) operator_id: String,
    pub(in crate::platforms::plugins::group_management) reason: String,
    pub(in crate::platforms::plugins::group_management) source: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(in crate::platforms::plugins::group_management) struct OffenderHistory {
    pub(in crate::platforms::plugins::group_management) user_id: String,
    pub(in crate::platforms::plugins::group_management) user_name: String,
    pub(in crate::platforms::plugins::group_management) ban_count: u64,
    pub(in crate::platforms::plugins::group_management) total_duration: u64,
    pub(in crate::platforms::plugins::group_management) first_ban_at: i64,
    pub(in crate::platforms::plugins::group_management) last_ban_at: i64,
    pub(in crate::platforms::plugins::group_management) last_reason: String,
    pub(in crate::platforms::plugins::group_management) reason_history: Vec<ReasonEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::platforms::plugins::group_management) struct ReasonEntry {
    pub(in crate::platforms::plugins::group_management) reason: String,
    pub(in crate::platforms::plugins::group_management) duration: u64,
    pub(in crate::platforms::plugins::group_management) banned_at: i64,
    pub(in crate::platforms::plugins::group_management) operator_id: String,
    pub(in crate::platforms::plugins::group_management) record_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::platforms::plugins::group_management) struct KickRecord {
    pub(in crate::platforms::plugins::group_management) record_id: String,
    pub(in crate::platforms::plugins::group_management) group_id: String,
    pub(in crate::platforms::plugins::group_management) user_id: String,
    pub(in crate::platforms::plugins::group_management) user_name: String,
    pub(in crate::platforms::plugins::group_management) kicked_at: i64,
    pub(in crate::platforms::plugins::group_management) operator_id: String,
    pub(in crate::platforms::plugins::group_management) reason: String,
    pub(in crate::platforms::plugins::group_management) reject_add_request: bool,
    pub(in crate::platforms::plugins::group_management) source: String,
}

/// 统一的群管理事件流：禁言/解禁/踢出/头衔追加到同一份持久化记录，
/// 查询与统计从这里读取。旧的 offender_history / kick_history 两个 key
/// 照常写入（WebUI 的 HTTP 端点依赖它们），查询时按 record_id 去重合并，
/// 事件流上线前的历史因此自动补齐。
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(in crate::platforms::plugins::group_management) struct ManagementEvent {
    pub(in crate::platforms::plugins::group_management) record_id: String,
    /// ban | unban | kick | kick_black | title_set | title_clear
    pub(in crate::platforms::plugins::group_management) action: String,
    pub(in crate::platforms::plugins::group_management) user_id: String,
    pub(in crate::platforms::plugins::group_management) user_name: String,
    #[serde(default)]
    pub(in crate::platforms::plugins::group_management) duration: u64,
    pub(in crate::platforms::plugins::group_management) happened_at: i64,
    pub(in crate::platforms::plugins::group_management) operator_id: String,
    pub(in crate::platforms::plugins::group_management) reason: String,
    pub(in crate::platforms::plugins::group_management) source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(in crate::platforms::plugins::group_management) detail: String,
}

pub(in crate::platforms::plugins::group_management) fn update_offender(
    context: &PlatformTurnContext,
    settings: &Settings,
    record: &BanRecord,
) -> Result<()> {
    context.state_store.plugin_update_json(
        &GroupManagementPlugin::scope(context),
        OFFENDERS_KEY,
        |current: Option<HashMap<String, OffenderHistory>>| {
            let mut map = current.unwrap_or_default();
            let entry = map
                .entry(record.user_id.clone())
                .or_insert_with(|| OffenderHistory {
                    user_id: record.user_id.clone(),
                    user_name: record.user_name.clone(),
                    first_ban_at: record.started_at,
                    ..OffenderHistory::default()
                });
            entry.user_name.clone_from(&record.user_name);
            entry.ban_count = entry.ban_count.saturating_add(1);
            entry.total_duration = entry.total_duration.saturating_add(record.duration);
            entry.last_ban_at = record.started_at;
            entry.last_reason.clone_from(&record.reason);
            entry.reason_history.push(ReasonEntry {
                reason: record.reason.clone(),
                duration: record.duration,
                banned_at: record.started_at,
                operator_id: record.operator_id.clone(),
                record_id: record.record_id.clone(),
            });
            // 外层 map 早有上限,但每个人的 reason_history 只 push 不裁——
            // 高频被禁者会一直累积,而这份记录是整块 JSON 读改写的,越长越
            // 慢也越占。裁掉最旧的,留最近的那批。
            trim_vec(&mut entry.reason_history, MAX_REASON_HISTORY_PER_OFFENDER);
            if map.len() > settings.max_offender_history_per_group {
                if let Some(remove) = map
                    .iter()
                    .min_by_key(|(_, item)| (item.ban_count, item.last_ban_at))
                    .map(|(id, _)| id.clone())
                {
                    map.remove(&remove);
                }
            }
            Ok(Some(map))
        },
    )?;
    Ok(())
}

pub(in crate::platforms::plugins::group_management) fn append_kick(
    context: &PlatformTurnContext,
    record: &KickRecord,
    max: usize,
) -> Result<()> {
    context.state_store.plugin_update_json(
        &GroupManagementPlugin::scope(context),
        KICKS_KEY,
        |current: Option<Vec<KickRecord>>| {
            let mut records = current.unwrap_or_default();
            records.push(record.clone());
            trim_vec(&mut records, max);
            Ok(Some(records))
        },
    )?;
    Ok(())
}

pub(in crate::platforms::plugins::group_management) fn append_event(
    context: &PlatformTurnContext,
    event: &ManagementEvent,
    max: usize,
) -> Result<()> {
    context.state_store.plugin_update_json(
        &GroupManagementPlugin::scope(context),
        EVENTS_KEY,
        |current: Option<Vec<ManagementEvent>>| {
            let mut events = current.unwrap_or_default();
            events.push(event.clone());
            trim_vec(&mut events, max);
            Ok(Some(events))
        },
    )?;
    Ok(())
}

/// 解析查询目标群：群聊内默认当前群；带 group_id 且非当前群时要求
/// Miyu 管理员；群聊之外（私聊/CLI）group_id 必填且仅限管理员。
pub(in crate::platforms::plugins::group_management) fn resolve_query_scope(
    args: &Value,
    context: &PlatformTurnContext,
) -> Result<PlatformPluginScopeKey> {
    let requested = args
        .get("group_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let current = GroupManagementPlugin::scope(context);
    let Some(id) = requested else {
        if context.conversation.kind == ConversationKind::Group {
            return Ok(current);
        }
        bail!("群聊之外查询必须提供 group_id");
    };
    if !valid_id(id) {
        bail!("group_id 必须是数字群号");
    }
    if context.conversation.kind == ConversationKind::Group
        && context.conversation.conversation_id == id
    {
        return Ok(current);
    }
    if !context.is_admin {
        bail!("跨群查询仅限 Miyu 管理员");
    }
    Ok(PlatformPluginScopeKey {
        conversation_kind: "group".to_string(),
        conversation_id: id.to_string(),
        ..current
    })
}

/// 汇总三个来源为一条按时间升序的事件流：新事件流为主，旧的
/// kick_history 与 offender_history.reason_history 按 record_id 去重补入，
/// 因此事件流上线之前的历史也查得到。
pub(in crate::platforms::plugins::group_management) fn load_all_events(
    context: &PlatformTurnContext,
    scope: &PlatformPluginScopeKey,
) -> Result<Vec<ManagementEvent>> {
    load_all_events_from(&context.state_store, scope)
}

pub(in crate::platforms::plugins::group_management) fn load_all_events_from(
    store: &crate::state::StateStore,
    scope: &PlatformPluginScopeKey,
) -> Result<Vec<ManagementEvent>> {
    let mut events = store
        .plugin_get_json::<Vec<ManagementEvent>>(scope, EVENTS_KEY)?
        .unwrap_or_default();
    let mut seen = events
        .iter()
        .map(|event| event.record_id.clone())
        .collect::<HashSet<_>>();
    let kicks = store
        .plugin_get_json::<Vec<KickRecord>>(scope, KICKS_KEY)?
        .unwrap_or_default();
    for kick in kicks {
        if seen.insert(kick.record_id.clone()) {
            events.push(ManagementEvent {
                record_id: kick.record_id,
                action: if kick.reject_add_request {
                    "kick_black"
                } else {
                    "kick"
                }
                .to_string(),
                user_id: kick.user_id,
                user_name: kick.user_name,
                duration: 0,
                happened_at: kick.kicked_at,
                operator_id: kick.operator_id,
                reason: kick.reason,
                source: kick.source,
                detail: String::new(),
            });
        }
    }
    let offenders = store
        .plugin_get_json::<HashMap<String, OffenderHistory>>(scope, OFFENDERS_KEY)?
        .unwrap_or_default();
    for offender in offenders.into_values() {
        for entry in &offender.reason_history {
            if seen.insert(entry.record_id.clone()) {
                events.push(ManagementEvent {
                    record_id: entry.record_id.clone(),
                    action: "ban".to_string(),
                    user_id: offender.user_id.clone(),
                    user_name: offender.user_name.clone(),
                    duration: entry.duration,
                    happened_at: entry.banned_at,
                    operator_id: entry.operator_id.clone(),
                    reason: entry.reason.clone(),
                    source: "offender_history".to_string(),
                    detail: String::new(),
                });
            }
        }
    }
    events.sort_by_key(|event| event.happened_at);
    Ok(events)
}

pub(in crate::platforms::plugins::group_management) fn action_matches(
    filter: &str,
    action: &str,
) -> bool {
    match filter {
        "all" => true,
        "ban" => matches!(action, "ban" | "unban"),
        "kick" => matches!(action, "kick" | "kick_black"),
        "title" => matches!(action, "title_set" | "title_clear"),
        _ => false,
    }
}

/// 每条禁言事件的当前状态：后续有解禁则 unmuted，后续被再次禁言覆盖则
/// overridden，否则按到期时间判 active/expired。输入必须按时间升序。
pub(in crate::platforms::plugins::group_management) fn ban_statuses(
    events: &[ManagementEvent],
    now: i64,
) -> HashMap<String, String> {
    let mut statuses = HashMap::new();
    for (index, event) in events.iter().enumerate() {
        if event.action != "ban" {
            continue;
        }
        let mut status = if event.happened_at.saturating_add(event.duration as i64) <= now {
            "expired"
        } else {
            "active"
        };
        for later in &events[index + 1..] {
            if later.user_id != event.user_id {
                continue;
            }
            match later.action.as_str() {
                "unban" => {
                    status = "unmuted";
                    break;
                }
                "ban" => {
                    status = "overridden";
                    break;
                }
                _ => {}
            }
        }
        statuses.insert(event.record_id.clone(), status.to_string());
    }
    statuses
}

pub(in crate::platforms::plugins::group_management) fn query_history(
    args: Value,
    context: &PlatformTurnContext,
) -> Result<String> {
    let scope = match resolve_query_scope(&args, context) {
        Ok(scope) => scope,
        Err(error) => return json_result(false, &error.to_string(), Value::Null),
    };
    let action = args.get("action").and_then(Value::as_str).unwrap_or("all");
    if !matches!(action, "ban" | "kick" | "title" | "all") {
        return json_result(false, "action 必须是 ban/kick/title/all", Value::Null);
    }
    let events = load_all_events(context, &scope)?;
    match args.get("view").and_then(Value::as_str).unwrap_or("events") {
        "stats" => query_history_stats(&args, action, &events, &scope.conversation_id),
        "events" => query_history_events(&args, action, events, &scope.conversation_id),
        _ => json_result(false, "view 必须是 events 或 stats", Value::Null),
    }
}

pub(in crate::platforms::plugins::group_management) fn query_history_events(
    args: &Value,
    action: &str,
    events: Vec<ManagementEvent>,
    group_id: &str,
) -> Result<String> {
    let user_id = optional_id(args);
    let keyword = lowercase_keyword(args);
    let ascending = args.get("sort_order").and_then(Value::as_str) == Some("asc");
    let statuses = ban_statuses(&events, now_unix());
    let mut records = events
        .into_iter()
        .filter(|event| action_matches(action, &event.action))
        .filter(|event| user_id.as_deref().is_none_or(|id| event.user_id == id))
        .filter(|event| {
            keyword.is_empty()
                || event.user_name.to_lowercase().contains(&keyword)
                || event.reason.to_lowercase().contains(&keyword)
                || event.detail.to_lowercase().contains(&keyword)
        })
        .map(|event| {
            let status = statuses.get(&event.record_id).cloned();
            (event, status)
        })
        .collect::<Vec<_>>();
    if !ascending {
        records.reverse();
    }
    records.truncate(limit(args));
    // 08-21 token-diet:逐条 JSON(实测 57% 结构开销)改为行格式;不可信的
    // 名字/理由/详情过 safe_prompt_field 防伪造记录行。
    let mut output = format!("group {group_id}: {} management event(s)\n", records.len());
    for (event, status) in &records {
        let mut line = format!(
            "[{}] {} {}(QQ:{})",
            format_event_time(event.happened_at),
            event.action,
            safe_prompt_field(&event.user_name),
            safe_prompt_field(&event.user_id),
        );
        if event.duration > 0 {
            line.push_str(&format!(" duration={}s", event.duration));
        }
        if let Some(status) = status {
            line.push_str(&format!(" status={status}"));
        }
        line.push_str(&format!(" by QQ:{}", safe_prompt_field(&event.operator_id)));
        if !event.reason.is_empty() {
            line.push_str(&format!(" reason: {}", safe_prompt_field(&event.reason)));
        }
        if !event.detail.is_empty() {
            line.push_str(&format!(" ({})", safe_prompt_field(&event.detail)));
        }
        output.push_str(&line);
        output.push('\n');
    }
    Ok(output)
}

fn format_event_time(timestamp: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(timestamp, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| timestamp.to_string())
}

#[derive(Default, Serialize)]
pub(in crate::platforms::plugins::group_management) struct MemberStats {
    pub(in crate::platforms::plugins::group_management) user_id: String,
    pub(in crate::platforms::plugins::group_management) user_name: String,
    pub(in crate::platforms::plugins::group_management) ban_count: u64,
    pub(in crate::platforms::plugins::group_management) total_ban_duration: u64,
    pub(in crate::platforms::plugins::group_management) kick_count: u64,
    pub(in crate::platforms::plugins::group_management) title_count: u64,
    pub(in crate::platforms::plugins::group_management) last_action_at: i64,
    pub(in crate::platforms::plugins::group_management) last_reason: String,
}

pub(in crate::platforms::plugins::group_management) fn aggregate_member_stats(
    action: &str,
    events: &[ManagementEvent],
) -> Vec<MemberStats> {
    let mut map: HashMap<String, MemberStats> = HashMap::new();
    for event in events
        .iter()
        .filter(|event| action_matches(action, &event.action))
    {
        let entry = map.entry(event.user_id.clone()).or_default();
        entry.user_id.clone_from(&event.user_id);
        if !event.user_name.is_empty() {
            entry.user_name.clone_from(&event.user_name);
        }
        match event.action.as_str() {
            "ban" => {
                entry.ban_count += 1;
                entry.total_ban_duration = entry.total_ban_duration.saturating_add(event.duration);
            }
            "kick" | "kick_black" => entry.kick_count += 1,
            "title_set" | "title_clear" => entry.title_count += 1,
            _ => {}
        }
        if event.happened_at >= entry.last_action_at {
            entry.last_action_at = event.happened_at;
            if !event.reason.is_empty() {
                entry.last_reason.clone_from(&event.reason);
            }
        }
    }
    map.into_values().collect()
}

pub(in crate::platforms::plugins::group_management) fn query_history_stats(
    args: &Value,
    action: &str,
    events: &[ManagementEvent],
    group_id: &str,
) -> Result<String> {
    let user_id = optional_id(args);
    let keyword = lowercase_keyword(args);
    let minimum = args
        .get("min_ban_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let sort_by = args
        .get("sort_by")
        .and_then(Value::as_str)
        .unwrap_or("ban_count");
    let ascending = args.get("sort_order").and_then(Value::as_str) == Some("asc");
    let mut items = aggregate_member_stats(action, events)
        .into_iter()
        .filter(|item| item.ban_count >= minimum)
        .filter(|item| user_id.as_deref().is_none_or(|id| item.user_id == id))
        .filter(|item| {
            keyword.is_empty()
                || item.user_name.to_lowercase().contains(&keyword)
                || item.last_reason.to_lowercase().contains(&keyword)
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| match sort_by {
        "kick_count" => item.kick_count as i64,
        "total_duration" => item.total_ban_duration as i64,
        "time" | "last_action_at" => item.last_action_at,
        _ => item.ban_count as i64,
    });
    if !ascending {
        items.reverse();
    }
    items.truncate(limit(args));
    let mut output = format!(
        "group {group_id}: {} member(s), sorted by {sort_by}\n",
        items.len()
    );
    for item in &items {
        output.push_str(&format!(
            "QQ {} {}: ban×{} total {}s, kick×{}, title×{}, last [{}]",
            safe_prompt_field(&item.user_id),
            safe_prompt_field(&item.user_name),
            item.ban_count,
            item.total_ban_duration,
            item.kick_count,
            item.title_count,
            format_event_time(item.last_action_at),
        ));
        if !item.last_reason.is_empty() {
            output.push_str(&format!(" {}", safe_prompt_field(&item.last_reason)));
        }
        output.push('\n');
    }
    Ok(output)
}

pub(in crate::platforms::plugins::group_management) fn lowercase_keyword(args: &Value) -> String {
    args.get("keyword")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase()
}

pub(in crate::platforms::plugins::group_management) async fn record_real_context(
    context: &PlatformTurnContext,
    id: &str,
    action: &str,
    member: &PlatformGroupMember,
    reason: &str,
    duration: Option<u64>,
) -> Result<()> {
    let mut text = format!(
        "[System:群管理行为]\n操作：{action}\n执行者：Miyu（{}）\n对象：{}（{}）",
        context.conversation.account_id,
        member.display_name(),
        member.user_id
    );
    if let Some(duration) = duration {
        text.push_str(&format!("\n时长：{duration} 秒"));
    }
    if !reason.is_empty() {
        text.push_str(&format!("\n原因：{reason}"));
    }
    text.push_str(&format!("\n记录 ID：{id}"));
    context
        .plugins
        .try_record_external_bot_message(context, &format!("qq-management-{id}"), &text)
        .await
}

/// 每个被禁者最多留多少条封禁理由。
///
/// 这份历史是给「屡犯判定」和历史查询用的：同一个人在同一个群里被禁 50 次，
/// 早就够判定了，更早的记录对决策没有增量。上限乘以群内被禁者上限（500）
/// 就是这份 JSON 的最坏体积。
pub(in crate::platforms::plugins::group_management) const MAX_REASON_HISTORY_PER_OFFENDER: usize =
    50;

pub(in crate::platforms::plugins::group_management) fn trim_vec<T>(
    values: &mut Vec<T>,
    max: usize,
) {
    let max = max.max(1);
    if values.len() > max {
        values.drain(..values.len() - max);
    }
}

pub(in crate::platforms::plugins::group_management) fn record_id() -> String {
    format!("{:012x}", rand::random::<u64>() & 0xffffffffffff)
}

pub(in crate::platforms::plugins::group_management) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    /// 外层 map 早有上限，但每个人的 reason_history 原来只 push 不裁——
    /// 同一个人被禁一千次就存一千条，而这份记录每次封禁都要整块 JSON
    /// 读改写，越长越慢也越占。
    #[test]
    fn one_offenders_reason_history_is_bounded() {
        let mut history = OffenderHistory::default();
        for index in 0..1_000 {
            history.reason_history.push(ReasonEntry {
                reason: format!("理由 {index}"),
                duration: 60,
                banned_at: index,
                operator_id: "op".to_string(),
                record_id: format!("{index:012x}"),
            });
            trim_vec(&mut history.reason_history, MAX_REASON_HISTORY_PER_OFFENDER);
        }
        assert_eq!(
            history.reason_history.len(),
            MAX_REASON_HISTORY_PER_OFFENDER
        );
        // 裁掉的是最旧的，留下的是最近那批
        assert_eq!(history.reason_history.last().unwrap().reason, "理由 999");
        assert_eq!(
            history.reason_history.first().unwrap().reason,
            format!("理由 {}", 1_000 - MAX_REASON_HISTORY_PER_OFFENDER)
        );
    }
}
