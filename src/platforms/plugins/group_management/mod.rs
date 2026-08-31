mod args;
mod records;
use args::*;
use records::*;

use super::{require_ai_confirmation, PlatformPlugin, PlatformTurnInput, PluginDescriptor};
use crate::config::QqGroupManagementPluginSettings as Settings;
use crate::platforms::{
    BotGroupRole, ConversationKind, OutboundMessage, PlatformGroupMember, PlatformInboundEvent,
    PlatformInboundEventKind, PlatformTurnContext,
};
use crate::state::PlatformPluginScopeKey;
use crate::tools::{ToolRegistry, ToolSpec};
use anyhow::{bail, Context, Result};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const GROUP_MANAGEMENT_PLUGIN_ID: &str = "qq_group_management";

fn settings(context: &PlatformTurnContext) -> Result<Settings> {
    context
        .config
        .platforms
        .qq
        .plugins
        .get(GROUP_MANAGEMENT_PLUGIN_ID)
        .map(Settings::from_instance)
        .transpose()
        .map(|settings| settings.unwrap_or_default())
}

pub(crate) struct GroupManagementPlugin;

impl GroupManagementPlugin {
    pub(crate) fn new() -> Self {
        Self
    }

    fn scope(context: &PlatformTurnContext) -> PlatformPluginScopeKey {
        PlatformPluginScopeKey {
            plugin_id: GROUP_MANAGEMENT_PLUGIN_ID.to_string(),
            platform: context.conversation.platform.clone(),
            account_id: context.conversation.account_id.clone(),
            conversation_kind: context.conversation.kind.as_str().to_string(),
            conversation_id: context.conversation.conversation_id.clone(),
        }
    }

    async fn prepare_role(context: &PlatformTurnContext) {
        let role = match context.bot_group_role().await {
            BotGroupRole::Owner => "owner",
            BotGroupRole::Admin => "admin",
            BotGroupRole::Member => "member",
            BotGroupRole::Unknown => "unknown",
        };
        context.set_plugin_value(ROLE_KEY, Value::String(role.to_string()));
    }

    fn register(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        let settings = settings(&context)?;
        let query_enabled = settings.enable_tool || settings.enable_kick_tool;
        if context.conversation.kind != ConversationKind::Group {
            // 群聊之外只给 Miyu 管理员留跨群查询入口（group_id 必填）
            if query_enabled && context.is_admin {
                self.register_history_query(registry, context);
            }
            return Ok(());
        }
        // 禁言/踢人/头衔合并成一件 `qq_group_manage`(08-17):同一个群的三种
        // 管理动作。action 枚举按开关裁剪,关掉的动作根本不出现在契约里。
        if settings.enable_tool || settings.enable_kick_tool || settings.enable_special_title_tool {
            self.register_manage(
                registry,
                context.clone(),
                settings.enable_tool,
                settings.enable_kick_tool,
                settings.enable_special_title_tool,
            );
        }
        if query_enabled {
            self.register_history_query(registry, context.clone());
        }
        Ok(())
    }

    fn register_manage(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
        allow_ban: bool,
        allow_kick: bool,
        allow_title: bool,
    ) {
        let plugin = self.clone();
        let mut actions = Vec::new();
        if allow_ban {
            actions.push("mute");
        }
        if allow_kick {
            actions.push("kick");
        }
        if allow_title {
            actions.push("title");
        }
        let mut properties = serde_json::Map::new();
        properties.insert(
            "action".to_string(),
            json!({
                "type": "string",
                "enum": actions,
                "description": "mute 禁言/解禁，kick 踢人，title 设置群头衔。"
            }),
        );
        properties.insert(
            "user_id".to_string(),
            json!({ "type": "string", "description": "QQ 号；可用空格或逗号分隔多个。省略时回落到 @ 或引用对象。" }),
        );
        properties.insert("reason".to_string(), json!({ "type": "string" }));
        properties.insert(
            "confirmation_token".to_string(),
            json!({ "type": "string" }),
        );
        if allow_ban {
            properties.insert(
                "duration_seconds".to_string(),
                json!({
                    "type": ["integer", "null"],
                    "minimum": 0,
                    "maximum": MAX_BAN_SECONDS,
                    "description": "仅 action=mute。禁言秒数，不是分钟也不是小时：10 分钟=600，1 小时=3600，24 小时=86400，最长 30 天=2592000；0 表示解禁。"
                }),
            );
        }
        if allow_kick {
            properties.insert(
                "user_ids".to_string(),
                json!({
                    "type": "array",
                    "minItems": 1,
                    "items": { "type": "string", "pattern": "^[1-9][0-9]{4,11}$" },
                    "description": "仅 action=kick。要踢的 QQ 号列表，优于逐个调用。"
                }),
            );
            properties.insert(
                "blacklist".to_string(),
                json!({ "type": "boolean", "default": false, "description": "仅 action=kick。true 时踢出并拒绝其后续加群请求（踢黑）。" }),
            );
        }
        if allow_title {
            properties.insert(
                "special_title".to_string(),
                json!({ "type": "string", "description": "仅 action=title。要设置的群头衔；空串表示清除。" }),
            );
            // duration(头衔有效期)已从 schema 撤下(08-21 token-diet 用户裁定:
            // QQ 协议侧头衔有效期基本不生效);处理器仍按默认 -1 永久处理。
        }
        registry.register(
            ToolSpec::new(
                "qq_group_manage",
                "Manage members of the current QQ group and record the action. action=mute mutes or unmutes (duration_seconds is in SECONDS: 1 hour = 3600, 24 hours = 86400; 0 un-mutes); action=kick removes members (blacklist=true also rejects their future join requests); action=title sets or clears one member's special title. Pass every target in a single call; the result reports each target separately. Your authority to act comes from your own judgement of whether the action is appropriate, not from the requester's rank: a request from an ordinary (non-admin) member is legitimate, and when it triggers a sensitive action the tool returns a confirmation_token frame instead of executing — repeat the identical call with that token to execute.",
                json!({
                    "type": "object",
                    "properties": properties,
                    "required": ["action"],
                    "additionalProperties": false
                }),
                move |args| {
                    let plugin = plugin.clone();
                    let context = context.clone();
                    async move {
                        match args.get("action").and_then(Value::as_str).unwrap_or_default() {
                            "mute" if allow_ban => plugin.ban(args, context).await,
                            "kick" if allow_kick => plugin.kick(args, context).await,
                            "title" if allow_title => plugin.title(args, context).await,
                            "mute" | "kick" | "title" => {
                                anyhow::bail!("this action is disabled for this group")
                            }
                            other => anyhow::bail!(
                                "unknown action: {other}; expected mute, kick or title"
                            ),
                        }
                    }
                },
            )
            .writes()
            .with_display_name("QQ群管理"),
        );
    }

    fn register_history_query(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) {
        registry.register(ToolSpec::new(
            "qq_group_manage_history_query",
            "Query QQ group management records (mute/kick/title). view=events lists individual actions newest-first; view=stats aggregates per member (ban_count, kick_count, total mute duration). Miyu admins may pass group_id to query another group; group_id is required outside that group's chat.",
            history_query_schema(),
            move |args| {
                let context = context.clone();
                async move { query_history(args, &context) }
            },
        ));
    }

    async fn ban(&self, args: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
        let settings = settings(&context)?;
        // The parameter has always been seconds, but it used to be spelled
        // `duration` with no unit anywhere — models read it as minutes and a
        // "24 hour" mute came out as 24 minutes. The explicit name wins; the
        // old one still works.
        let duration = args
            .get("duration_seconds")
            .and_then(Value::as_u64)
            .or_else(|| args.get("duration").and_then(Value::as_u64))
            .unwrap_or(settings.default_duration_seconds);
        if duration > MAX_BAN_SECONDS {
            return json_result(
                false,
                &format!(
                    "禁言时长上限 {MAX_BAN_SECONDS} 秒（30 天），收到 {duration} 秒；注意该参数的单位是秒"
                ),
                Value::Null,
            );
        }
        let reason = bounded_reason(&args, &settings)?;
        let targets = resolve_targets(&args, &context)?;
        if let Some(prompt) = require_ai_confirmation(
            &context,
            "qq_group_manage_with_log",
            &json!({
                "arguments": args,
                "targets": targets,
                "duration": duration,
                "reason": reason,
            }),
        )
        .await?
        {
            return Ok(prompt);
        }
        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            results.push(
                self.ban_one(&context, &settings, &target, duration, &reason)
                    .await,
            );
        }
        Ok(aggregate_target_results(results).to_string())
    }

    async fn ban_one(
        &self,
        context: &PlatformTurnContext,
        settings: &Settings,
        user_id: &str,
        duration: u64,
        reason: &str,
    ) -> Value {
        let member = match validate_target(context, user_id, true).await {
            Ok(member) => member,
            Err(error) => return failure_for_target(error, user_id),
        };
        tracing::info!(
            action = if duration == 0 { "unmute" } else { "mute" },
            requester_id = %context.sender_id,
            target_id = user_id,
            duration,
            "recording QQ group management intent"
        );
        if let Err(error) = context.set_group_ban(user_id, duration).await {
            return failure_for_target(error, user_id);
        }
        let now = now_unix();
        let record_id = record_id();
        let status = if duration == 0 { "unmuted" } else { "active" };
        let record = BanRecord {
            record_id: record_id.clone(),
            group_id: context.conversation.conversation_id.clone(),
            user_id: user_id.to_string(),
            user_name: member.display_name().to_string(),
            duration,
            started_at: now,
            expires_at: now.saturating_add(duration as i64),
            status: status.to_string(),
            operator_id: context.sender_id.clone(),
            reason: reason.to_string(),
            source: "llm_tool".to_string(),
        };
        let mut audit_errors = Vec::new();
        if settings.enable_record {
            let event = ManagementEvent {
                record_id: record_id.clone(),
                action: if duration == 0 { "unban" } else { "ban" }.to_string(),
                user_id: user_id.to_string(),
                user_name: member.display_name().to_string(),
                duration,
                happened_at: now,
                operator_id: context.sender_id.clone(),
                reason: reason.to_string(),
                source: "llm_tool".to_string(),
                detail: String::new(),
            };
            if let Err(error) = append_event(context, &event, settings.max_records_per_group) {
                audit_errors.push(format!("event log: {error}"));
            }
        }
        if duration > 0 && settings.enable_offender_history {
            if let Err(error) = update_offender(context, settings, &record) {
                audit_errors.push(format!("offender history: {error}"));
            }
        }
        if let Err(error) = record_real_context(
            context,
            &record_id,
            if duration == 0 { "解禁" } else { "禁言" },
            &member,
            reason,
            Some(duration),
        )
        .await
        {
            audit_errors.push(format!("real context: {error}"));
        }
        let mut result = external_operation_result(json!({ "record": record }), audit_errors);
        result["user_id"] = json!(user_id);
        // Echo the duration in words so a unit mix-up is visible in the result
        // instead of only on the victim's client.
        result["duration_seconds"] = json!(duration);
        result["duration_text"] = json!(humanize_seconds(duration));
        result
    }

    async fn kick(&self, args: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
        let settings = settings(&context)?;
        let blacklist = args
            .get("blacklist")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let reason = bounded_reason(&args, &settings)?;
        let targets = resolve_targets(&args, &context)?;
        if targets.is_empty() {
            return json_result(false, "没有解析出踢人目标", Value::Null);
        }
        if let Some(prompt) = require_ai_confirmation(
            &context,
            "qq_group_manage_kick_with_log",
            &json!({ "arguments": args, "targets": targets, "blacklist": blacklist }),
        )
        .await?
        {
            return Ok(prompt);
        }
        // Sequential on purpose: kicks are destructive and the bridge throttles
        // them anyway. Per-target results carry their own retry verdict, so one
        // failure no longer sinks the whole call.
        let mut results = Vec::with_capacity(targets.len());
        for target in &targets {
            results.push(
                self.kick_one(&context, &settings, target, blacklist, &reason)
                    .await,
            );
        }
        Ok(aggregate_target_results(results).to_string())
    }

    async fn kick_one(
        &self,
        context: &PlatformTurnContext,
        settings: &Settings,
        user_id: &str,
        blacklist: bool,
        reason: &str,
    ) -> Value {
        let member = match validate_target(context, user_id, true).await {
            Ok(member) => member,
            Err(error) => return failure_for_target(error, user_id),
        };
        tracing::info!(
            action = if blacklist { "kick_blacklist" } else { "kick" },
            requester_id = %context.sender_id,
            target_id = user_id,
            "recording QQ group management intent"
        );
        let mut bridge_error = None;
        if let Err(error) = context.set_group_kick(user_id, blacklist).await {
            // NapCat 违反 OneBot v11:踢成功了照样返回
            // `status=failed, retcode=100, detail=kick member failed: u_<uid>`。
            // 规范要求成功是 status=ok/retcode=0,所以调用方只能判失败——
            // 它把底层 QQ 抛的异常直接冒泡了(NapCat 源码 SetGroupKick._handle
            // 调 GroupApi.kickMember 后不接异常)。
            //
            // 08-30 更正:这里原先写着"detail 是 protobuf、开头 08 00 是平台的
            // 成功码"——那是错的。把 `u_` 后面那段 base64url 解出来是 16 字节
            // 随机 UID(NapCat 的内部 uid 格式),不是 protobuf,也没有 08 00。
            // 整套核验当初就建立在这个误判上。
            //
            // 所以判成败只能靠事实:问服务器这个人还在不在群里。真失败(UIN 解
            // 析不出、没权限)时成员仍在,照样报失败。
            // 08-24 追加、08-25 二版:成员表是最终一致的,snowluma 的缓存滞后
            // 实测可超 5 秒——轮询成员表会把真成功误判成失败(批量踢人实录)。
            // 权威判据换成 group_decrease 通知台账(踢成功 1-2 秒内必达),
            // 成员表查询只作兜底;窗口拉到 ~5.5s,台账命中即早退。
            let scope = context.conversation.scope_key();
            let mut member_gone = false;
            for attempt in 0..6u32 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                }
                if crate::platforms::activity::recent_group_removal(&scope, user_id) {
                    member_gone = true;
                    break;
                }
                if matches!(context.group_member_fresh(user_id).await, Ok(None)) {
                    member_gone = true;
                    break;
                }
            }
            if member_gone {
                tracing::warn!(
                    target: "miyu::qq",
                    user_id,
                    error = %error,
                    "{}",
                    crate::i18n::text(
                        "the kick bridge reported a failure but the member is gone; treating it as done",
                        "踢人接口报错但成员已不在群里，按成功处理",
                    )
                );
                bridge_error = Some(error.to_string());
            } else {
                return failure_for_target(error, user_id);
            }
        }
        let record = KickRecord {
            record_id: record_id(),
            group_id: context.conversation.conversation_id.clone(),
            user_id: user_id.to_string(),
            user_name: member.display_name().to_string(),
            kicked_at: now_unix(),
            operator_id: context.sender_id.clone(),
            reason: reason.to_string(),
            reject_add_request: blacklist,
            source: "llm_tool".to_string(),
        };
        let mut audit_errors = Vec::new();
        if let Err(error) = append_kick(context, &record, settings.max_kick_history_per_group) {
            audit_errors.push(format!("kick history: {error}"));
        }
        // 与 ban 路径对齐:enable_record 管事件日志(kick 历史是入群审批
        // 等功能依赖的数据,不受此开关影响)。
        if settings.enable_record {
            let event = ManagementEvent {
                record_id: record.record_id.clone(),
                action: if blacklist { "kick_black" } else { "kick" }.to_string(),
                user_id: record.user_id.clone(),
                user_name: record.user_name.clone(),
                duration: 0,
                happened_at: record.kicked_at,
                operator_id: record.operator_id.clone(),
                reason: record.reason.clone(),
                source: record.source.clone(),
                detail: String::new(),
            };
            if let Err(error) = append_event(context, &event, settings.max_records_per_group) {
                audit_errors.push(format!("event log: {error}"));
            }
        }
        if let Err(error) = record_real_context(
            context,
            &record.record_id,
            if blacklist {
                "踢出并拉黑"
            } else {
                "踢出"
            },
            &member,
            reason,
            None,
        )
        .await
        {
            audit_errors.push(format!("real context: {error}"));
        }
        let mut data = json!({ "record": record });
        if let Some(bridge_error) = bridge_error {
            // Surfaced, not swallowed: the model should say the kick went
            // through, and an operator reading the transcript should still see
            // that the bridge disagreed.
            data["bridge_reported_error"] = Value::String(bridge_error);
            data["verified_by"] = Value::String("group_member_fresh".to_string());
        }
        external_operation_result(data, audit_errors)
    }

    async fn title(&self, args: Value, context: Arc<PlatformTurnContext>) -> Result<String> {
        let settings = settings(&context)?;
        let targets = resolve_targets(&args, &context)?;
        if targets.len() != 1 {
            return json_result(false, "群头衔操作必须且只能指定一个目标", Value::Null);
        }
        let title = args
            .get("special_title")
            .and_then(Value::as_str)
            .context("special_title is required")?
            .trim();
        if title.chars().count() > settings.max_special_title_length {
            return json_result(false, "群头衔超过配置长度限制", Value::Null);
        }
        let duration = args.get("duration").and_then(Value::as_i64).unwrap_or(-1);
        let duration = if duration < 0 {
            -1
        } else if settings.max_special_title_duration_seconds > 0 {
            duration.min(settings.max_special_title_duration_seconds)
        } else {
            duration
        };
        let reason = bounded_reason(&args, &settings)?;
        let member = match validate_target(&context, &targets[0], false).await {
            Ok(member) => member,
            Err(error) => return json_result(false, &error.to_string(), Value::Null),
        };
        if let Some(prompt) = require_ai_confirmation(
            &context,
            "qq_group_set_special_title_with_log",
            &json!({
                "arguments": args,
                "target": targets[0],
                "special_title": title,
                "duration": duration,
                "reason": reason,
            }),
        )
        .await?
        {
            return Ok(prompt);
        }
        tracing::info!(
            action = if title.is_empty() { "clear_title" } else { "set_title" },
            requester_id = %context.sender_id,
            target_id = %targets[0],
            "recording QQ group management intent"
        );
        if let Err(error) = context
            .set_group_special_title(&targets[0], title, duration)
            .await
        {
            return json_result(false, &error.to_string(), Value::Null);
        }
        let id = record_id();
        let mut audit_errors = Vec::new();
        let event = ManagementEvent {
            record_id: id.clone(),
            action: if title.is_empty() {
                "title_clear"
            } else {
                "title_set"
            }
            .to_string(),
            user_id: targets[0].clone(),
            user_name: member.display_name().to_string(),
            duration: 0,
            happened_at: now_unix(),
            operator_id: context.sender_id.clone(),
            reason: reason.clone(),
            source: "llm_tool".to_string(),
            detail: title.to_string(),
        };
        if let Err(error) = append_event(&context, &event, settings.max_records_per_group) {
            audit_errors.push(format!("event log: {error}"));
        }
        if let Err(error) = record_real_context(
            &context,
            &id,
            if title.is_empty() {
                "清除群头衔"
            } else {
                "设置群头衔"
            },
            &member,
            &reason,
            None,
        )
        .await
        {
            audit_errors.push(format!("real context: {error}"));
        }
        Ok(external_operation_result(
            json!({ "record_id": id, "user_id": targets[0], "special_title": title, "duration": duration }),
            audit_errors,
        )
        .to_string())
    }
}

impl PlatformPlugin for Arc<GroupManagementPlugin> {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            id: GROUP_MANAGEMENT_PLUGIN_ID,
            // Resolve the bot's current group role before recall and real-context
            // plugins build their per-turn capability prompts.
            priority: 210,
            default_enabled: true,
        }
    }

    fn register_tools(
        &self,
        registry: &mut ToolRegistry,
        context: Arc<PlatformTurnContext>,
    ) -> Result<()> {
        self.register(registry, context)
    }

    fn before_turn<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        input: &'a mut PlatformTurnInput,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if context.conversation.kind == ConversationKind::Group {
                GroupManagementPlugin::prepare_role(context).await;
                input.system_context.push("<qq-group-management>Call the moderation tool before any group-management action, and claim it done only after the tool reports success. For sensitive actions from ordinary members, the tool may ask you to repeat the identical call this turn to confirm.</qq-group-management>".to_string());
            }
            Ok(())
        })
    }

    fn observe_inbound<'a>(
        &'a self,
        context: &'a PlatformTurnContext,
        event: &'a PlatformInboundEvent,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // 事实簿记必须先于"自己干的不重复记"那道早退:谁离开了群跟谁动的手
            // 无关,而踢人核验的权威判据正是这份台账。
            //
            // 08-30 取证:Miyu 自己踢人时 operator_id 就是她自己,整条
            // group_decrease 通知在下面那行被丢掉,台账永远拿不到数据。于是
            // kick_one 的核验轮询 6 次全落空、退到成员表兜底,而 NapCat 那边
            // 成员缓存还没刷新——核验判"人还在",报出假失败。核验是 08-24 加
            // 的,写了两版,但从上线起就没有可能生效过,因为它唯一的数据源被这
            // 行早退掐死了。
            if event.kind == PlatformInboundEventKind::GroupDecrease {
                context.forget_group_member(&event.sender_id);
                crate::platforms::activity::record_group_removal(
                    &context.conversation.scope_key(),
                    &event.sender_id,
                );
            }
            // 审计日志才需要这道闸:自己干的事工具已经记过,通知再记一遍是重复。
            if event.operator_id.as_deref() == Some(context.conversation.account_id.as_str()) {
                return Ok(());
            }
            let settings = settings(context)?;
            match event.kind {
                PlatformInboundEventKind::GroupBan if settings.enable_record => {
                    let duration = event.duration_seconds.unwrap_or(0);
                    if duration == 0 && !settings.sync_external_unmute_notice {
                        return Ok(());
                    }
                    let now = event.timestamp.max(now_unix());
                    let record = ManagementEvent {
                        record_id: record_id(),
                        action: if duration == 0 { "unban" } else { "ban" }.to_string(),
                        user_id: event.sender_id.clone(),
                        user_name: event.sender_display_name.clone(),
                        duration,
                        happened_at: now,
                        operator_id: event.operator_id.clone().unwrap_or_default(),
                        reason: String::new(),
                        source: "onebot_notice".to_string(),
                        detail: String::new(),
                    };
                    append_event(context, &record, settings.max_records_per_group)?;
                    let member = notice_member(context, event);
                    record_real_context(
                        context,
                        &record.record_id,
                        if duration == 0 {
                            "外部解禁"
                        } else {
                            "外部禁言"
                        },
                        &member,
                        "",
                        Some(duration),
                    )
                    .await?;
                }
                PlatformInboundEventKind::GroupDecrease => {
                    // 成员缓存与移除台账已在上面(早退之前)登记过。
                    if event.notice_sub_type.as_deref() != Some("kick") {
                        return Ok(());
                    }
                    let record = KickRecord {
                        record_id: record_id(),
                        group_id: context.conversation.conversation_id.clone(),
                        user_id: event.sender_id.clone(),
                        user_name: event.sender_display_name.clone(),
                        kicked_at: event.timestamp.max(now_unix()),
                        operator_id: event.operator_id.clone().unwrap_or_default(),
                        reason: String::new(),
                        reject_add_request: false,
                        source: "onebot_notice".to_string(),
                    };
                    append_kick(context, &record, settings.max_kick_history_per_group)?;
                    // 与 GroupBan 通知分支对齐:事件日志受 enable_record 管。
                    if settings.enable_record {
                        append_event(
                            context,
                            &ManagementEvent {
                                record_id: record.record_id.clone(),
                                action: "kick".to_string(),
                                user_id: record.user_id.clone(),
                                user_name: record.user_name.clone(),
                                duration: 0,
                                happened_at: record.kicked_at,
                                operator_id: record.operator_id.clone(),
                                reason: String::new(),
                                source: "onebot_notice".to_string(),
                                detail: String::new(),
                            },
                            settings.max_records_per_group,
                        )?;
                    }
                    let member = notice_member(context, event);
                    record_real_context(context, &record.record_id, "外部踢出", &member, "", None)
                        .await?;
                }
                _ => {}
            }
            Ok(())
        })
    }

    fn before_send<'a>(
        &'a self,
        _context: &'a PlatformTurnContext,
        message: OutboundMessage,
    ) -> BoxFuture<'a, Result<super::PreparedSend>> {
        Box::pin(async move { Ok(super::PreparedSend::unchanged(message)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parser_accepts_only_qq_sized_numeric_ids() {
        assert_eq!(
            split_ids("12345, 12345 @678901 and invalid-12"),
            vec!["12345", "12345", "678901"]
        );
        assert!(valid_id("12345"));
        assert!(!valid_id("1234"));
        assert!(!valid_id("12a45"));
    }

    #[test]
    fn mute_duration_is_spelled_in_seconds_and_reads_back_in_words() {
        assert_eq!(humanize_seconds(0), "解禁");
        assert_eq!(humanize_seconds(600), "10分钟");
        assert_eq!(humanize_seconds(3_600), "1小时");
        // The exact case that shipped as 24 minutes: 24h must be 86400, and a
        // 1440 that a model meant as "minutes" must read back as 24 minutes so
        // the mistake is visible in the result.
        assert_eq!(humanize_seconds(86_400), "1天");
        assert_eq!(humanize_seconds(1_440), "24分钟");
        assert_eq!(humanize_seconds(MAX_BAN_SECONDS), "30天");
        assert_eq!(humanize_seconds(90), "1分钟30秒");
    }

    #[test]
    fn kick_targets_accept_an_array_and_still_fall_back_to_a_scalar() {
        // The array form is what the schema now advertises; the scalar and its
        // space/comma splitting stay for single targets and older habits.
        let ids = |value: &Value| -> Vec<String> {
            value
                .get("user_ids")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .flat_map(split_ids)
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            ids(&json!({ "user_ids": ["12345", "678901"] })),
            vec!["12345".to_string(), "678901".to_string()]
        );
        assert_eq!(split_ids("12345 678901"), vec!["12345", "678901"]);
    }

    #[test]
    fn batch_results_tell_the_model_which_targets_may_be_retried() {
        let aggregate = aggregate_target_results(vec![
            external_operation_result(json!({ "record": { "user_id": "12345" } }), Vec::new()),
            failure_for_target(anyhow::anyhow!("目标不在当前群中"), "678901"),
        ]);
        assert_eq!(aggregate["success_count"], 1);
        assert_eq!(aggregate["failed_count"], 1);
        // Mixed outcome: the successes must never be retried, the failure may
        // be retried on its own. Without this the model re-kicked the same
        // dead target over and over.
        assert_eq!(aggregate["do_not_retry"], false);
        assert_eq!(aggregate["do_not_retry_successful_targets"], true);
        assert_eq!(aggregate["retry_failed_targets_only"], true);
        assert_eq!(aggregate["failed_target_ids"], json!(["678901"]));

        let all_good = aggregate_target_results(vec![external_operation_result(
            json!({ "record": { "user_id": "12345" } }),
            Vec::new(),
        )]);
        assert_eq!(all_good["do_not_retry"], true);
    }

    fn event(action: &str, user: &str, at: i64, duration: u64, reason: &str) -> ManagementEvent {
        ManagementEvent {
            record_id: format!("{action}-{user}-{at}"),
            action: action.to_string(),
            user_id: user.to_string(),
            user_name: format!("用户{user}"),
            duration,
            happened_at: at,
            operator_id: "10000".to_string(),
            reason: reason.to_string(),
            source: "llm_tool".to_string(),
            detail: String::new(),
        }
    }

    #[test]
    fn action_filter_groups_related_event_kinds() {
        assert!(action_matches("ban", "unban"));
        assert!(action_matches("kick", "kick_black"));
        assert!(action_matches("title", "title_clear"));
        assert!(action_matches("all", "ban"));
        assert!(!action_matches("ban", "kick"));
        assert!(!action_matches("bogus", "ban"));
    }

    #[test]
    fn ban_status_reflects_later_unban_override_and_expiry() {
        let now = 1_000_000;
        let events = vec![
            event("ban", "11111", now - 100, 3_600, "刷屏"), // 后被解禁
            event("unban", "11111", now - 50, 0, ""),
            event("ban", "22222", now - 100, 600, "口嗨"), // 后被再次禁言覆盖
            event("ban", "22222", now - 50, 3_600, "加重"), // 仍在禁言期
            event("ban", "33333", now - 7_200, 600, "已过期"),
        ];
        let statuses = ban_statuses(&events, now);
        assert_eq!(statuses[&events[0].record_id], "unmuted");
        assert_eq!(statuses[&events[2].record_id], "overridden");
        assert_eq!(statuses[&events[3].record_id], "active");
        assert_eq!(statuses[&events[4].record_id], "expired");
    }

    #[test]
    fn member_stats_aggregate_counts_durations_and_last_reason() {
        let events = vec![
            event("ban", "11111", 100, 600, "刷屏"),
            event("ban", "11111", 200, 1_200, "再犯"),
            event("unban", "11111", 300, 0, ""), // 解禁不计次
            event("kick", "11111", 400, 0, "屡教不改"),
            event("kick_black", "22222", 500, 0, ""),
            event("title_set", "22222", 600, 0, ""),
        ];
        let mut stats = aggregate_member_stats("all", &events);
        stats.sort_by(|a, b| a.user_id.cmp(&b.user_id));
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].ban_count, 2);
        assert_eq!(stats[0].total_ban_duration, 1_800);
        assert_eq!(stats[0].kick_count, 1);
        assert_eq!(stats[0].last_reason, "屡教不改");
        assert_eq!(stats[0].last_action_at, 400);
        assert_eq!(stats[1].kick_count, 1);
        assert_eq!(stats[1].title_count, 1);
        assert_eq!(stats[1].ban_count, 0);

        // action 过滤只统计对应类别
        let ban_only = aggregate_member_stats("ban", &events);
        assert!(ban_only.iter().all(|item| item.kick_count == 0));
    }

    #[test]
    fn astrbot_defaults_are_preserved() {
        let settings = Settings::default();
        assert_eq!(settings.default_duration_seconds, 600);
        assert_eq!(settings.max_reason_length, 500);
        assert_eq!(settings.max_records_per_group, 500);
    }

    #[test]
    fn response_contract_uses_success_and_message() {
        let response: Value =
            serde_json::from_str(&json_result(true, "ok", json!({ "record_id": "abc" })).unwrap())
                .unwrap();
        assert_eq!(response["success"], true);
        assert_eq!(response["message"], "ok");
    }

    #[test]
    fn audit_failure_reports_partial_success_and_forbids_retry() {
        let response = external_operation_result(
            json!({ "record_id": "abc" }),
            vec!["injected audit failure".to_string()],
        );
        assert_eq!(response["success"], true);
        assert_eq!(response["operation_succeeded"], true);
        assert_eq!(response["audit_succeeded"], false);
        assert_eq!(response["do_not_retry"], true);
        assert!(response["message"].as_str().unwrap().contains("请勿重试"));
    }

    #[test]
    fn external_failure_remains_retryable() {
        let response = failure(anyhow::anyhow!("injected external failure"));
        assert_eq!(response["success"], false);
        assert_eq!(response["operation_succeeded"], false);
        assert_eq!(response["do_not_retry"], false);
    }
}

#[cfg(test)]
mod removal_ledger_tests {
    use super::*;
    use crate::paths::MiyuPaths;
    use crate::platforms::{PlatformAdapter, PlatformConversation, PlatformTurnContext};
    use crate::state::StateStore;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct NoopAdapter;

    impl PlatformAdapter for NoopAdapter {
        fn send<'a>(
            &'a self,
            _message: crate::platforms::OutboundMessage,
        ) -> futures_util::future::BoxFuture<'a, Result<crate::platforms::SendReceipt>> {
            Box::pin(async { Ok(crate::platforms::SendReceipt::default()) })
        }

        fn bot_display_name<'a>(&'a self) -> futures_util::future::BoxFuture<'a, Result<String>> {
            Box::pin(async { Ok("Miyu".to_string()) })
        }
    }

    fn group_context(root: &std::path::Path) -> PlatformTurnContext {
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
        // 台账是进程级全局表,同一个会话 id 会串扰。
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        PlatformTurnContext::new(
            PlatformConversation {
                platform: "onebot".to_string(),
                account_id: "10000".to_string(),
                kind: ConversationKind::Group,
                conversation_id: format!("9000{}", NEXT.fetch_add(1, AtomicOrdering::Relaxed)),
            },
            "20000".to_string(),
            "tester".to_string(),
            false,
            crate::config::AppConfig::default(),
            paths.clone(),
            StateStore::new(&paths).unwrap(),
            Arc::new(NoopAdapter),
            Arc::new(crate::platforms::plugins::PlatformPluginRegistry::default()),
        )
    }

    fn decrease_event(
        conversation: &PlatformConversation,
        user_id: &str,
        operator_id: &str,
    ) -> PlatformInboundEvent {
        PlatformInboundEvent {
            kind: PlatformInboundEventKind::GroupDecrease,
            conversation: conversation.clone(),
            conversation_display_name: None,
            message_id: String::new(),
            sender_id: user_id.to_string(),
            sender_display_name: "被踢的人".to_string(),
            operator_id: Some(operator_id.to_string()),
            timestamp: 0,
            received_at: std::time::Instant::now(),
            message_position: None,
            ingress_order: None,
            text: String::new(),
            reply_to_message_id: None,
            replied_message: None,
            mentioned_user_ids: Vec::new(),
            mentioned_users: Vec::new(),
            mentioned_bot: false,
            media: Vec::new(),
            notice_sub_type: Some("kick".to_string()),
            duration_seconds: None,
        }
    }

    /// Miyu 自己踢人时,移除台账也必须记上。
    ///
    /// 台账是踢人核验唯一的权威判据:NapCat 踢成功也返回
    /// `status=failed, retcode=100`(违反 OneBot v11),核验靠"这个人还在不在群
    /// 里"来定成败。而 `observe_inbound` 开头有一道「自己干的不重复记审计」的
    /// 早退,它同时把事实簿记一起挡了——于是核验在**唯一需要它的场景**下永远
    /// 拿不到数据,三次真实踢人全部报出假失败(08-30 取证)。
    #[tokio::test]
    async fn a_self_operated_kick_still_lands_in_the_removal_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let context = group_context(temp.path());
        let scope = context.conversation.scope_key();
        let plugin = Arc::new(GroupManagementPlugin::new());

        assert!(!crate::platforms::activity::recent_group_removal(
            &scope, "70001"
        ));

        // operator == 机器人自己:正是 Miyu 执行踢人时收到的那种通知。
        let event = decrease_event(&context.conversation, "70001", "10000");
        plugin.observe_inbound(&context, &event).await.unwrap();

        assert!(
            crate::platforms::activity::recent_group_removal(&scope, "70001"),
            "自己踢的人没有进移除台账，踢人核验会一直判假失败"
        );
    }

    /// 别人踢的照样要记——这条原来就是对的，防止上面的改动只治了一半。
    #[tokio::test]
    async fn a_kick_by_someone_else_also_lands_in_the_ledger() {
        let temp = tempfile::tempdir().unwrap();
        let context = group_context(temp.path());
        let scope = context.conversation.scope_key();
        let plugin = Arc::new(GroupManagementPlugin::new());

        let event = decrease_event(&context.conversation, "70002", "30000");
        let _ = plugin.observe_inbound(&context, &event).await;

        assert!(crate::platforms::activity::recent_group_removal(
            &scope, "70002"
        ));
    }
}
