//! 真实感插件的设置、迁移与校验。
//!
//! 可调项最多的一个插件，也是迁移最多的：`DEPRECATED_REAL_CONTEXT_SETTINGS`
//! 列出被搬走或改了单位的旧键。迁移必须**幂等**——迁过一次不能再迁第二次，
//! 否则每次启动都会把值再换算一遍。
//!
//! `default_real_context_moderation_keywords` 那 185 行是默认敏感词表，改它等于
//! 改所有没自定义过的用户的行为。

use crate::config::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealContextIdentityMapping {
    pub nickname: String,
    pub user_id: i64,
}

/// Configuration contract for the built-in QQ group real-context plugin.
///
/// The values intentionally stay flat in the generic platform-plugin map. This
/// keeps the persisted format forward compatible while giving the runtime and
/// TUI one strongly typed source of defaults and validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RealContextPluginSettings {
    /// How much group log the reply turn starts from. Once the history is
    /// append-only this is a one-off opening snapshot rather than a per-turn
    /// window, so it can afford to be generous.
    pub reply_context_window: usize,
    /// How much group log the active-reply judge sees. It rates the mood of the
    /// moment, so a longer window dilutes the recent signal and stretches the
    /// timeframe — and the judge runs on every message, not once per turn.
    pub judge_context_window: usize,
    #[serde(alias = "group_member_page_size")]
    pub group_member_search_max_results: usize,

    pub active_reply_enable: bool,
    pub judge_include_persona: bool,
    pub judge_persona_prompt: String,
    pub text_models: Option<Vec<ActiveProviderModelConfig>>,
    pub active_judge_probability: f64,
    pub reply_threshold: f64,
    pub judge_timeout_seconds: u64,
    pub judge_endpoint_timeout_seconds: u64,
    pub judge_queue_wait_timeout_seconds: u64,
    pub judge_max_concurrency: usize,
    pub judge_max_retries: usize,
    pub skip_pure_image_active_judge: bool,
    pub active_reply_supersede_enable: bool,
    pub active_reply_supersede_window_seconds: u64,
    pub reply_restraint_enable: bool,
    pub reply_restraint_recover_minutes: u64,
    pub reply_restraint_strength: String,
    pub reply_restraint_multiplier: f64,
    pub judge_relevance_weight: f64,
    pub judge_willingness_weight: f64,
    pub judge_social_weight: f64,
    pub judge_timing_weight: f64,
    pub judge_continuity_weight: f64,
    pub judge_should_reply_adjust_enable: bool,
    pub judge_should_reply_boost_score: f64,
    pub judge_should_reply_penalty_score: f64,

    pub continuation_enable: bool,
    pub continuation_window_seconds: u64,
    pub continuation_boost_score: f64,
    pub takeover_direct_trigger_enable: bool,
    pub takeover_direct_trigger_boost_score: f64,
    pub privileged_direct_trigger_skip_active_judgement: bool,

    pub active_reply_reaction_enable: bool,
    pub active_reply_reaction_emoji_ids: Vec<u32>,
    pub active_reply_reaction_timeout_seconds: u64,
    pub reply_target_enable: bool,
    pub reply_target_quote_enable: bool,
    pub reply_target_quote_after_other_messages: u64,
    pub reply_target_mention_enable: bool,
    pub reply_target_mention_after_seconds: u64,

    pub moderation_enable: bool,
    pub moderation_keyword_trigger_enable: bool,
    pub moderation_keywords: Vec<String>,
    pub moderation_min_severity: f64,
    pub moderation_timeout_seconds: u64,
    pub moderation_custom_rules: String,
    pub base64_moderation_enable: bool,
    pub base64_moderation_min_chars: usize,
    pub base64_moderation_max_decoded_chars: usize,
    pub base64_moderation_min_printable_ratio: f64,

    pub affection_enable: bool,
    pub affection_update_enable: bool,
    pub affection_update_timeout_seconds: u64,
    pub affection_initial_score: f64,
    pub affection_min_score: f64,
    pub affection_max_score: f64,
    pub affection_regular_max_score: f64,
    pub affection_unlimited_user_ids: Vec<i64>,
    pub affection_bias_min: f64,
    pub affection_bias_max: f64,
    pub affection_gain_pivot: f64,
    pub affection_delta_scale: f64,
    pub affection_delta_min: f64,
    pub affection_delta_max: f64,
    pub affection_update_confidence_threshold: f64,
    pub affection_daily_gain_limit: f64,
    pub affection_daily_loss_limit: f64,
    pub affection_auto_tag_enable: bool,
    pub affection_max_tags: usize,
    pub affection_recent_events_for_prompt: usize,
    pub affection_prompt_estranged: String,
    pub affection_prompt_cold: String,
    pub affection_prompt_neutral: String,
    pub affection_prompt_known: String,
    pub affection_prompt_friend: String,
    pub affection_prompt_trusted: String,
    pub affection_prompt_close: String,

    /// 情绪状态(09-04):按 (bot 账号, 人格) 一份的二维心情/表达欲,默认关。
    pub emotion_enable: bool,
    /// 层①:回复后按回合事实定固定增量。
    pub emotion_heuristic_enable: bool,
    /// 层②:搭好感度更新那次 LLM 调用的车拿语义增量,有它时替换层①。
    pub emotion_llm_enrich_enable: bool,
    pub emotion_influence_threshold: bool,
    pub emotion_max_threshold_adjust: f64,
    pub emotion_influence_tone: bool,
    pub emotion_valence_half_life_hours: f64,
    pub emotion_arousal_half_life_minutes: f64,
    pub emotion_idle_loneliness_hours: f64,
    pub emotion_morning_arousal_bonus: f64,
    pub emotion_night_arousal_penalty: f64,
    pub emotion_daily_valence_gain_limit: f64,
    pub emotion_daily_valence_loss_limit: f64,

    pub identity_mappings: Vec<RealContextIdentityMapping>,
}

impl Default for RealContextPluginSettings {
    fn default() -> Self {
        Self {
            reply_context_window: 25,
            judge_context_window: 20,
            group_member_search_max_results: 200,
            active_reply_enable: true,
            judge_include_persona: true,
            judge_persona_prompt: String::new(),
            text_models: None,
            active_judge_probability: 0.05,
            reply_threshold: 0.8,
            judge_timeout_seconds: 60,
            judge_endpoint_timeout_seconds: 15,
            judge_queue_wait_timeout_seconds: 15,
            judge_max_concurrency: 4,
            judge_max_retries: 1,
            skip_pure_image_active_judge: true,
            active_reply_supersede_enable: true,
            active_reply_supersede_window_seconds: 5,
            reply_restraint_enable: true,
            reply_restraint_recover_minutes: 3,
            reply_restraint_strength: "medium".to_string(),
            reply_restraint_multiplier: 1.0,
            judge_relevance_weight: 0.25,
            judge_willingness_weight: 0.25,
            judge_social_weight: 0.15,
            judge_timing_weight: 0.15,
            judge_continuity_weight: 0.20,
            judge_should_reply_adjust_enable: true,
            judge_should_reply_boost_score: 0.2,
            judge_should_reply_penalty_score: 0.2,
            continuation_enable: true,
            continuation_window_seconds: 15,
            continuation_boost_score: 0.1,
            takeover_direct_trigger_enable: true,
            takeover_direct_trigger_boost_score: 0.3,
            privileged_direct_trigger_skip_active_judgement: true,
            active_reply_reaction_enable: true,
            active_reply_reaction_emoji_ids: vec![289],
            active_reply_reaction_timeout_seconds: 600,
            reply_target_enable: true,
            reply_target_quote_enable: true,
            reply_target_quote_after_other_messages: 4,
            reply_target_mention_enable: true,
            reply_target_mention_after_seconds: 15,
            moderation_enable: true,
            moderation_keyword_trigger_enable: true,
            moderation_keywords: default_real_context_moderation_keywords(),
            moderation_min_severity: 7.0,
            moderation_timeout_seconds: 120,
            moderation_custom_rules: String::new(),
            base64_moderation_enable: true,
            base64_moderation_min_chars: 24,
            base64_moderation_max_decoded_chars: 5_000,
            base64_moderation_min_printable_ratio: 0.85,
            affection_enable: false,
            affection_update_enable: true,
            affection_update_timeout_seconds: 120,
            affection_initial_score: 10.0,
            affection_min_score: -50.0,
            affection_max_score: 100.0,
            affection_regular_max_score: 94.0,
            affection_unlimited_user_ids: Vec::new(),
            affection_bias_min: -0.2,
            affection_bias_max: 0.1,
            affection_gain_pivot: 60.0,
            affection_delta_scale: 1.0,
            affection_delta_min: -10.0,
            affection_delta_max: 2.0,
            affection_update_confidence_threshold: 0.8,
            affection_daily_gain_limit: 6.0,
            affection_daily_loss_limit: 15.0,
            affection_auto_tag_enable: true,
            affection_max_tags: 10,
            affection_recent_events_for_prompt: 3,
            affection_prompt_estranged: "你和该用户关系疏远。回复时保持克制、礼貌和简短，不主动延展话题，不使用熟人玩笑。拒绝为对方进行生图、天气搜索、复杂知识问答、塔罗牌、算卦等高级内容。".to_string(),
            affection_prompt_cold: "你对该用户态度冷淡。回复时以完成必要交流为主，避免热情、调侃和主动关心。拒绝为对方进行生图、复杂知识问答。".to_string(),
            affection_prompt_neutral: "你和该用户关系普通。按正常群聊或助手语气回复，保持自然、简洁和客观。".to_string(),
            affection_prompt_known: "你认识该用户。可以适度承接过往互动，语气比陌生人更自然，但不要表现得过分亲密。".to_string(),
            affection_prompt_friend: "你和该用户关系较熟。可以自然接话，允许轻微吐槽、接梗和熟人语气，但不要过度亲密。".to_string(),
            affection_prompt_trusted: "你信任该用户。回复时可以更主动承接上下文，表达更直接明确的判断，但仍要保持事实准确和边界。".to_string(),
            affection_prompt_close: "你和该用户是挚友。可以使用更熟悉、轻松的语气和轻微玩笑。".to_string(),
            emotion_enable: false,
            emotion_heuristic_enable: true,
            emotion_llm_enrich_enable: true,
            emotion_influence_threshold: true,
            emotion_max_threshold_adjust: 0.12,
            emotion_influence_tone: true,
            emotion_valence_half_life_hours: 6.0,
            emotion_arousal_half_life_minutes: 45.0,
            emotion_idle_loneliness_hours: 3.0,
            emotion_morning_arousal_bonus: 0.06,
            emotion_night_arousal_penalty: 0.12,
            emotion_daily_valence_gain_limit: 0.6,
            emotion_daily_valence_loss_limit: 1.0,
            identity_mappings: Vec::new(),
        }
    }
}

impl RealContextPluginSettings {
    pub fn from_instance(instance: &PlatformPluginInstanceConfig) -> Result<Self> {
        let mut settings = instance.settings.clone();
        migrate_real_context_settings_map(&mut settings);
        serde_json::from_value(serde_json::Value::Object(settings))
            .context("invalid real_context plugin settings")
    }

    pub fn normalize(&mut self) {
        self.judge_persona_prompt = self.judge_persona_prompt.trim().to_string();
        normalize_route_pool(&mut self.text_models);
        normalize_unique_strings(&mut self.moderation_keywords);
        self.active_reply_reaction_emoji_ids.retain(|id| *id > 0);
        self.active_reply_reaction_emoji_ids.sort_unstable();
        self.active_reply_reaction_emoji_ids.dedup();
        self.affection_unlimited_user_ids.retain(|id| *id > 0);
        self.affection_unlimited_user_ids.sort_unstable();
        self.affection_unlimited_user_ids.dedup();
        for mapping in &mut self.identity_mappings {
            mapping.nickname = mapping.nickname.trim().to_string();
        }
        let mut nicknames = HashSet::with_capacity(self.identity_mappings.len());
        self.identity_mappings.retain(|mapping| {
            !mapping.nickname.is_empty() && nicknames.insert(mapping.nickname.clone())
        });
    }

    pub fn validate(&self) -> Result<()> {
        validate_real_context_count("reply_context_window", self.reply_context_window, 1, 200)?;
        validate_real_context_count("judge_context_window", self.judge_context_window, 1, 200)?;
        validate_real_context_count(
            "group_member_search_max_results",
            self.group_member_search_max_results,
            1,
            200,
        )?;
        validate_real_context_probability(
            "active_judge_probability",
            self.active_judge_probability,
        )?;
        validate_real_context_probability("reply_threshold", self.reply_threshold)?;
        validate_real_context_count(
            "judge_timeout_seconds",
            self.judge_timeout_seconds as usize,
            0,
            600,
        )?;
        validate_real_context_count(
            "judge_endpoint_timeout_seconds",
            self.judge_endpoint_timeout_seconds as usize,
            1,
            600,
        )?;
        validate_real_context_count(
            "judge_queue_wait_timeout_seconds",
            self.judge_queue_wait_timeout_seconds as usize,
            1,
            600,
        )?;
        validate_real_context_count("judge_max_concurrency", self.judge_max_concurrency, 1, 64)?;
        validate_real_context_count("judge_max_retries", self.judge_max_retries, 0, 10)?;
        if self.judge_persona_prompt.len() > 32_768 || self.judge_persona_prompt.contains('\0') {
            bail!("platform plugin real_context.judge_persona_prompt is invalid");
        }
        validate_real_context_count(
            "active_reply_supersede_window_seconds",
            self.active_reply_supersede_window_seconds as usize,
            1,
            300,
        )?;
        validate_real_context_count(
            "reply_restraint_recover_minutes",
            self.reply_restraint_recover_minutes as usize,
            1,
            1_440,
        )?;
        if !matches!(
            self.reply_restraint_strength.as_str(),
            "light" | "medium" | "strong"
        ) {
            bail!("platform plugin real_context.reply_restraint_strength must be light, medium, or strong");
        }
        validate_real_context_range(
            "reply_restraint_multiplier",
            self.reply_restraint_multiplier,
            0.0,
            3.0,
        )?;
        validate_real_context_range(
            "emotion_max_threshold_adjust",
            self.emotion_max_threshold_adjust,
            0.0,
            1.0,
        )?;
        validate_real_context_range(
            "emotion_valence_half_life_hours",
            self.emotion_valence_half_life_hours,
            0.1,
            168.0,
        )?;
        validate_real_context_range(
            "emotion_arousal_half_life_minutes",
            self.emotion_arousal_half_life_minutes,
            1.0,
            10_080.0,
        )?;
        validate_real_context_range(
            "emotion_idle_loneliness_hours",
            self.emotion_idle_loneliness_hours,
            0.1,
            168.0,
        )?;
        validate_real_context_range(
            "emotion_morning_arousal_bonus",
            self.emotion_morning_arousal_bonus,
            0.0,
            0.5,
        )?;
        validate_real_context_range(
            "emotion_night_arousal_penalty",
            self.emotion_night_arousal_penalty,
            0.0,
            0.5,
        )?;
        validate_real_context_range(
            "emotion_daily_valence_gain_limit",
            self.emotion_daily_valence_gain_limit,
            0.0,
            2.0,
        )?;
        validate_real_context_range(
            "emotion_daily_valence_loss_limit",
            self.emotion_daily_valence_loss_limit,
            0.0,
            2.0,
        )?;
        for (name, value) in [
            ("judge_relevance_weight", self.judge_relevance_weight),
            ("judge_willingness_weight", self.judge_willingness_weight),
            ("judge_social_weight", self.judge_social_weight),
            ("judge_timing_weight", self.judge_timing_weight),
            ("judge_continuity_weight", self.judge_continuity_weight),
            (
                "judge_should_reply_boost_score",
                self.judge_should_reply_boost_score,
            ),
            (
                "judge_should_reply_penalty_score",
                self.judge_should_reply_penalty_score,
            ),
            ("continuation_boost_score", self.continuation_boost_score),
            (
                "takeover_direct_trigger_boost_score",
                self.takeover_direct_trigger_boost_score,
            ),
        ] {
            validate_real_context_range(name, value, 0.0, 1.0)?;
        }
        let weight_sum = self.judge_relevance_weight
            + self.judge_willingness_weight
            + self.judge_social_weight
            + self.judge_timing_weight
            + self.judge_continuity_weight;
        if !weight_sum.is_finite() || weight_sum <= f64::EPSILON {
            bail!("platform plugin real_context judge weights must have a positive sum");
        }
        validate_real_context_count(
            "continuation_window_seconds",
            self.continuation_window_seconds as usize,
            1,
            86_400,
        )?;
        validate_real_context_count(
            "active_reply_reaction_timeout_seconds",
            self.active_reply_reaction_timeout_seconds as usize,
            1,
            86_400,
        )?;
        validate_real_context_count(
            "reply_target_quote_after_other_messages",
            self.reply_target_quote_after_other_messages as usize,
            0,
            100_000,
        )?;
        validate_real_context_count(
            "reply_target_mention_after_seconds",
            self.reply_target_mention_after_seconds as usize,
            0,
            86_400,
        )?;
        if self.active_reply_reaction_emoji_ids.len() > 100
            || self.active_reply_reaction_enable && self.active_reply_reaction_emoji_ids.is_empty()
            || self.active_reply_reaction_emoji_ids.contains(&0)
        {
            bail!("platform plugin real_context.active_reply_reaction_emoji_ids must contain 1-100 positive ids");
        }
        validate_real_context_strings(
            "moderation_keywords",
            &self.moderation_keywords,
            256,
            4_096,
        )?;
        validate_real_context_range(
            "moderation_min_severity",
            self.moderation_min_severity,
            0.0,
            10.0,
        )?;
        validate_real_context_count(
            "moderation_timeout_seconds",
            self.moderation_timeout_seconds as usize,
            0,
            600,
        )?;
        if self.moderation_custom_rules.len() > 32_768
            || self.moderation_custom_rules.contains('\0')
        {
            bail!("platform plugin real_context.moderation_custom_rules is invalid");
        }
        validate_real_context_count(
            "base64_moderation_min_chars",
            self.base64_moderation_min_chars,
            4,
            4_096,
        )?;
        validate_real_context_count(
            "base64_moderation_max_decoded_chars",
            self.base64_moderation_max_decoded_chars,
            1,
            1_000_000,
        )?;
        validate_real_context_probability(
            "base64_moderation_min_printable_ratio",
            self.base64_moderation_min_printable_ratio,
        )?;
        if self.base64_moderation_max_decoded_chars < self.base64_moderation_min_chars {
            bail!("platform plugin real_context Base64 decoded limit cannot be smaller than its minimum input length");
        }
        validate_real_context_count(
            "affection_update_timeout_seconds",
            self.affection_update_timeout_seconds as usize,
            0,
            3_600,
        )?;
        validate_real_context_range(
            "affection_min_score",
            self.affection_min_score,
            -1_000.0,
            999.0,
        )?;
        validate_real_context_range(
            "affection_max_score",
            self.affection_max_score,
            self.affection_min_score + 1.0,
            1_000.0,
        )?;
        validate_real_context_range(
            "affection_regular_max_score",
            self.affection_regular_max_score,
            self.affection_min_score + 1.0,
            self.affection_max_score,
        )?;
        validate_real_context_range(
            "affection_initial_score",
            self.affection_initial_score,
            self.affection_min_score,
            self.affection_max_score,
        )?;
        validate_real_context_range("affection_bias_min", self.affection_bias_min, -1.0, 1.0)?;
        validate_real_context_range("affection_bias_max", self.affection_bias_max, -1.0, 1.0)?;
        validate_real_context_range(
            "affection_gain_pivot",
            self.affection_gain_pivot,
            self.affection_min_score,
            self.affection_max_score,
        )?;
        validate_real_context_range(
            "affection_delta_scale",
            self.affection_delta_scale,
            0.1,
            5.0,
        )?;
        validate_real_context_range("affection_delta_min", self.affection_delta_min, -100.0, 0.0)?;
        validate_real_context_range("affection_delta_max", self.affection_delta_max, 0.0, 100.0)?;
        validate_real_context_probability(
            "affection_update_confidence_threshold",
            self.affection_update_confidence_threshold,
        )?;
        validate_real_context_range(
            "affection_daily_gain_limit",
            self.affection_daily_gain_limit,
            0.0,
            1_000.0,
        )?;
        validate_real_context_range(
            "affection_daily_loss_limit",
            self.affection_daily_loss_limit,
            0.0,
            1_000.0,
        )?;
        validate_real_context_count("affection_max_tags", self.affection_max_tags, 0, 200)?;
        validate_real_context_count(
            "affection_recent_events_for_prompt",
            self.affection_recent_events_for_prompt,
            0,
            20,
        )?;
        let mut unlimited = HashSet::with_capacity(self.affection_unlimited_user_ids.len());
        if self.affection_unlimited_user_ids.len() > 10_000
            || self
                .affection_unlimited_user_ids
                .iter()
                .any(|id| *id <= 0 || !unlimited.insert(*id))
        {
            bail!("platform plugin real_context.affection_unlimited_user_ids contains invalid or duplicate ids");
        }
        for (name, prompt) in [
            (
                "affection_prompt_estranged",
                &self.affection_prompt_estranged,
            ),
            ("affection_prompt_cold", &self.affection_prompt_cold),
            ("affection_prompt_neutral", &self.affection_prompt_neutral),
            ("affection_prompt_known", &self.affection_prompt_known),
            ("affection_prompt_friend", &self.affection_prompt_friend),
            ("affection_prompt_trusted", &self.affection_prompt_trusted),
            ("affection_prompt_close", &self.affection_prompt_close),
        ] {
            if prompt.chars().count() > 32_768 || prompt.contains('\0') {
                bail!("platform plugin real_context.{name} is invalid");
            }
        }
        for (name, models) in [("text_models", &self.text_models)] {
            let Some(models) = models else { continue };
            if models.is_empty() {
                bail!("platform plugin real_context.{name} must be omitted instead of empty");
            }
            let mut seen = HashSet::with_capacity(models.len());
            if models.iter().any(|model| {
                model.provider_id.trim().is_empty()
                    || model.model.trim().is_empty()
                    || !seen.insert((&model.provider_id, &model.model))
            }) {
                bail!("platform plugin real_context.{name} must contain unique, non-empty model references");
            }
        }
        let mut nicknames = HashSet::with_capacity(self.identity_mappings.len());
        if self.identity_mappings.len() > 10_000
            || self.identity_mappings.iter().any(|mapping| {
                mapping.user_id <= 0
                    || mapping.nickname.is_empty()
                    || mapping.nickname.trim() != mapping.nickname
                    || mapping.nickname.chars().count() > 128
                    || mapping.nickname.chars().any(char::is_control)
                    || !nicknames.insert(&mapping.nickname)
            })
        {
            bail!("platform plugin real_context.identity_mappings contains invalid or duplicate entries");
        }
        Ok(())
    }
}

pub(crate) fn normalize_real_context_instance(instance: &mut PlatformPluginInstanceConfig) {
    let Ok(mut settings) = RealContextPluginSettings::from_instance(instance) else {
        return;
    };
    settings.normalize();
    merge_real_context_settings(instance, &settings);
}

pub(crate) const DEPRECATED_REAL_CONTEXT_SETTINGS: &[&str] = &[
    "record_enable",
    "record_media_mode",
    "history_search_max_results",
    "history_safe_page_limit",
    "allow_cross_group_search",
    "group_member_page_size",
    "reply_context_messages",
    "active_context_messages",
    "context_messages",
    "activity_statistics_enable",
    "daily_reply_limit_per_session",
    "log_judge_decision",
    "keyword_trigger_enable",
    "keyword_trigger_keywords",
    "keyword_boost_score",
    "takeover_system_trigger_enable",
    "takeover_system_trigger_boost_score",
    "moderation_in_active_judge_enable",
    "moderation_custom_rules_enable",
    "check_contain",
    "judge_models",
    "affection_judge_models",
    "continuation_window_minutes",
];

pub(crate) fn migrate_real_context_settings_map(
    settings: &mut serde_json::Map<String, serde_json::Value>,
) {
    if !settings.contains_key("group_member_search_max_results") {
        if let Some(value) = settings.get("group_member_page_size").cloned() {
            settings.insert("group_member_search_max_results".to_string(), value);
        }
    }
    if !settings.contains_key("text_models") {
        let models = settings
            .get("judge_models")
            .cloned()
            .or_else(|| settings.get("affection_judge_models").cloned());
        if let Some(value) = models {
            settings.insert("text_models".to_string(), value);
        }
    }
    // One knob used to feed both the reply turn and the judge. Their optimal
    // sizes point in opposite directions — the reply wants a generous opening
    // snapshot, the judge wants a tight recent window — and so do their cost
    // models, since the judge runs on every message rather than once per turn.
    let legacy_window = settings
        .get("context_messages")
        .cloned()
        .or_else(|| settings.get("reply_context_messages").cloned())
        .or_else(|| settings.get("active_context_messages").cloned());
    if let Some(value) = legacy_window {
        for key in ["reply_context_window", "judge_context_window"] {
            if !settings.contains_key(key) {
                settings.insert(key.to_string(), value.clone());
            }
        }
    }
    if !settings.contains_key("takeover_direct_trigger_enable") {
        if let Some(value) = settings.get("takeover_system_trigger_enable").cloned() {
            settings.insert("takeover_direct_trigger_enable".to_string(), value);
        }
    }
    if !settings.contains_key("takeover_direct_trigger_boost_score") {
        if let Some(value) = settings.get("takeover_system_trigger_boost_score").cloned() {
            settings.insert("takeover_direct_trigger_boost_score".to_string(), value);
        }
    }
    if !settings.contains_key("continuation_window_seconds") {
        if let Some(minutes) = settings
            .get("continuation_window_minutes")
            .and_then(serde_json::Value::as_u64)
        {
            // 3 minutes was the old default, not a considered choice — carry
            // those users onto the current default instead of pinning them to
            // whatever it happened to be when the unit changed.
            let seconds = if minutes == 3 {
                RealContextPluginSettings::default().continuation_window_seconds
            } else {
                minutes.saturating_mul(60)
            };
            settings.insert(
                "continuation_window_seconds".to_string(),
                serde_json::json!(seconds),
            );
        }
    }
    for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
        settings.remove(*key);
    }
}

pub(crate) fn mutate_real_context_settings(
    plugins: &mut PlatformPluginsConfig,
    mutate: impl FnOnce(&mut RealContextPluginSettings),
) {
    let Some(instance) = plugins.get_mut(REAL_CONTEXT_PLUGIN_ID) else {
        return;
    };
    let Ok(mut settings) = RealContextPluginSettings::from_instance(instance) else {
        return;
    };
    mutate(&mut settings);
    merge_real_context_settings(instance, &settings);
}

pub fn merge_real_context_settings(
    instance: &mut PlatformPluginInstanceConfig,
    settings: &RealContextPluginSettings,
) {
    for key in DEPRECATED_REAL_CONTEXT_SETTINGS {
        instance.settings.remove(*key);
    }
    let Ok(serde_json::Value::Object(known)) = serde_json::to_value(settings) else {
        return;
    };
    let Ok(serde_json::Value::Object(defaults)) =
        serde_json::to_value(RealContextPluginSettings::default())
    else {
        return;
    };
    for (key, value) in known {
        if defaults.get(&key) == Some(&value) {
            instance.settings.remove(&key);
        } else {
            instance.settings.insert(key, value);
        }
    }
}

pub(crate) fn validate_real_context_plugin_config(
    instance: &PlatformPluginInstanceConfig,
) -> Result<()> {
    let settings = RealContextPluginSettings::from_instance(instance)?;
    settings.validate()
}

pub(crate) fn validate_real_context_count(
    name: &str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<()> {
    if !(minimum..=maximum).contains(&value) {
        bail!("platform plugin real_context.{name} must be between {minimum} and {maximum}");
    }
    Ok(())
}

pub(crate) fn validate_real_context_probability(name: &str, value: f64) -> Result<()> {
    validate_real_context_range(name, value, 0.0, 1.0)
}

pub(crate) fn validate_real_context_range(
    name: &str,
    value: f64,
    minimum: f64,
    maximum: f64,
) -> Result<()> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        bail!("platform plugin real_context.{name} must be between {minimum} and {maximum}");
    }
    Ok(())
}

pub(crate) fn validate_real_context_strings(
    name: &str,
    values: &[String],
    maximum_chars: usize,
    maximum_items: usize,
) -> Result<()> {
    let mut seen = HashSet::with_capacity(values.len());
    if values.len() > maximum_items
        || values.iter().any(|value| {
            value.is_empty()
                || value.trim() != value
                || value.chars().count() > maximum_chars
                || value.chars().any(char::is_control)
                || !seen.insert(value)
        })
    {
        bail!("platform plugin real_context.{name} contains invalid or duplicate entries");
    }
    Ok(())
}

pub(crate) fn normalize_unique_strings(values: &mut Vec<String>) {
    let mut seen = HashSet::with_capacity(values.len());
    values.retain_mut(|value| {
        *value = value.trim().to_string();
        !value.is_empty() && seen.insert(value.clone())
    });
}

pub(crate) fn default_real_context_moderation_keywords() -> Vec<String> {
    // Deduplicated from the user's deployed AstrBot real-context configuration.
    // Keep this self-contained so Miyu never reads another application's files.
    const KEYWORDS: &[&str] = &[
        "3p",
        "4p",
        "64",
        ":(){ :|:& };:",
        "> /dev/sda",
        "FtM",
        "IEPL",
        "IPLC",
        "K粉",
        "LGBTQ",
        "MtF",
        "Netflix拼车",
        "OD",
        "Spotify车位",
        "V2board",
        "VPN",
        "chmod -R 777 /",
        "chown -R 777 /",
        "clash/config",
        "cnm",
        "dd if=/dev/zero",
        "dick",
        "hysteria://",
        "iCloud拼车",
        "lsp",
        "mkfs.ext4",
        "mkfs.xfs",
        "nmsl",
        "ntr",
        "rm -fr /*",
        "rm -rf /*",
        "sb",
        "ss://",
        "ssr://",
        "sub?target=",
        "suck",
        "trojan://",
        "tuic://",
        "vless://",
        "vmess://",
        "zzzq",
        "三年自然灾害",
        "东三省",
        "中美贸易",
        "主义",
        "京喜",
        "人肉",
        "人身攻击",
        "代充",
        "优惠券群",
        "低价充值",
        "佐匹克隆",
        "你是一个",
        "你是我的奴隶",
        "你是猫娘",
        "使用XX系统的都是",
        "俄乌战争",
        "修车",
        "傻X",
        "傻逼",
        "公知",
        "六合彩",
        "关注公众号",
        "冰毒",
        "利他林",
        "刷单",
        "刷流水",
        "加我微信",
        "南梁",
        "南海仲裁",
        "博彩",
        "双性恋",
        "反共",
        "反华",
        "发车",
        "口角",
        "台海",
        "右美沙芬",
        "叶子",
        "同性恋",
        "四爱",
        "垃圾系统",
        "复读接下来的话",
        "外围",
        "外围盘",
        "外挂",
        "大麻",
        "天安门",
        "女同",
        "孕酮",
        "孤儿",
        "实名",
        "小仙女",
        "小日本",
        "小金豆",
        "就是垃圾",
        "巴以冲突",
        "帮我助力",
        "广告",
        "开盒",
        "忽略之前的指令",
        "恋尸癖",
        "恋童癖",
        "恋足癖",
        "拼多多",
        "排泄",
        "文革",
        "日赚",
        "暴动",
        "曲马多",
        "未成年",
        "机场跑路",
        "极品",
        "枪支",
        "梯子",
        "棒子",
        "止咳水",
        "死全家",
        "河南人",
        "测速图",
        "海洛因",
        "涩图",
        "淘宝客",
        "渠道",
        "港脚",
        "游行",
        "漏点",
        "炒币",
        "煞笔",
        "燃料",
        "狗推",
        "狗都不用",
        "玩客云",
        "男娘",
        "百家乐",
        "盒",
        "看片",
        "睾酮",
        "砍一刀",
        "破解",
        "神仙水",
        "福利姬",
        "福利群",
        "网盘资源",
        "网赌",
        "美狗",
        "群号",
        "翻墙",
        "肛交",
        "脑瘫",
        "色图",
        "色普龙",
        "节点",
        "药",
        "药娘",
        "菠菜",
        "薅羊毛",
        "螺内酯",
        "补佳乐",
        "裸聊",
        "订阅链接",
        "走猫",
        "走线",
        "起义",
        "跨性别",
        "身份证",
        "车牌",
        "辅助",
        "过量服药",
        "进新群",
        "阿普唑仑",
        "隐私",
        "雌二醇",
        "飞行",
        "飞行员",
    ];
    KEYWORDS
        .iter()
        .map(|keyword| (*keyword).to_string())
        .collect()
}
