//! REPL 底部那一行状态。
//!
//! 显示当前模式、provider/模型、思考变体，以及 token 计量：本轮用量、上下文
//! 占用与窗口、会话累计与缓存命中率。窄终端下要按优先级丢弃——模型名比累计
//! 数字重要，模式标签又比模型名重要。

use crate::cli::repl::width::*;
use crate::cli::*;

#[derive(Clone, Debug)]
pub(in crate::cli) struct ReplFooterStatus {
    pub(in crate::cli) provider: String,
    pub(in crate::cli) model: String,
    pub(in crate::cli) mixed_models: bool,
    pub(in crate::cli) thinking: Option<String>,
    pub(in crate::cli) token_usage: render::TokenMeter,
    /// 回合运行中的盲文转轮帧号;None=空闲不显示。随 spinner tick 推进,
    /// set_footer 的权威覆盖(from_config 构造)自然回落 None。
    pub(in crate::cli) running_spinner: Option<usize>,
}

/// Σ is hidden entirely when nothing has been spent yet, so an empty session
/// does not carry a "Σ0" that means nothing.
pub(in crate::cli) fn meter_cumulative(cumulative: TurnTokens) -> render::TokenMeter {
    render::TokenMeter {
        cumulative_tokens: (cumulative.total > 0).then_some(cumulative.total),
        cumulative_prompt_tokens: cumulative.prompt,
        cumulative_cached_tokens: cumulative.cache_read,
        ..Default::default()
    }
}

impl ReplFooterStatus {
    pub(in crate::cli) fn from_config(
        config: &AppConfig,
        session_tokens: u64,
        cumulative: TurnTokens,
    ) -> Self {
        let active = config.active_provider_model_choices();
        let mixed_models = active.len() > 1;
        let (provider_id, model) = match active.as_slice() {
            [] => ("-".to_string(), t("None", "无").to_string()),
            [choice] => (
                choice.provider_id.clone(),
                short_model_name(&choice.model, &choice.provider_id),
            ),
            _ => ("mixed".to_string(), t("Mixed", "混合").to_string()),
        };

        let window = config.active_context_window_with_source().ok().flatten();
        Self {
            model,
            provider: provider_id,
            mixed_models,
            thinking: None,
            running_spinner: None,
            token_usage: render::TokenMeter {
                session_tokens,
                context_window: window.map(|(value, _)| value),
                context_window_assumed: matches!(
                    window,
                    Some((_, crate::config::ContextWindowSource::Assumed))
                ),
                ..meter_cumulative(cumulative)
            },
        }
    }

    pub(in crate::cli) fn update_token_usage(
        &mut self,
        result: &crate::llm::ChatResult,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        if result.usage.is_some() {
            let turn = TurnTokens::from_usage(result.usage.as_ref());
            self.set_token_usage_with_cache(turn, session_tokens, context_window, cumulative);
        }
    }

    pub(in crate::cli) fn set_token_usage(
        &mut self,
        turn_tokens: u64,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        self.set_token_usage_with_cache(
            TurnTokens {
                total: turn_tokens,
                ..TurnTokens::default()
            },
            session_tokens,
            context_window,
            cumulative,
        );
    }

    pub(in crate::cli) fn set_token_usage_with_cache(
        &mut self,
        turn: TurnTokens,
        session_tokens: u64,
        context_window: Option<usize>,
        cumulative: TurnTokens,
    ) {
        self.token_usage = render::TokenMeter {
            turn_tokens: turn.total,
            turn_prompt_tokens: turn.prompt,
            turn_cached_tokens: turn.cache_read,
            session_tokens,
            context_window,
            ..meter_cumulative(cumulative)
        };
    }

    pub(in crate::cli) fn update_session_tokens(&mut self, session_tokens: u64) {
        self.token_usage.session_tokens = session_tokens;
    }

    /// 回合中途的逐请求刷新:在(回合前的)基线上叠加回合累计。必须作用
    /// 在基线快照的克隆上,同一回合内可重复调用而不重复相加。
    pub(in crate::cli) fn apply_round_usage(&mut self, context_tokens: u64, turn: TurnTokens) {
        let meter = &mut self.token_usage;
        meter.turn_tokens = turn.total;
        meter.turn_prompt_tokens = turn.prompt;
        meter.turn_cached_tokens = turn.cache_read;
        if context_tokens > 0 {
            meter.session_tokens = context_tokens;
        }
        let cumulative = meter.cumulative_tokens.unwrap_or(0) + turn.total;
        meter.cumulative_tokens = (cumulative > 0).then_some(cumulative);
        meter.cumulative_prompt_tokens += turn.prompt;
        meter.cumulative_cached_tokens += turn.cache_read;
    }

    /// `assumed` 必须跟着窗口值一起传：只更新数字、不更新出处，footer 就会拿
    /// 上一次的出处去解释这一次的数——切个会话或换个模型就错了。
    pub(in crate::cli) fn update_context_window(
        &mut self,
        context_window: Option<usize>,
        assumed: bool,
    ) {
        self.token_usage.context_window = context_window;
        self.token_usage.context_window_assumed = assumed;
    }

    /// Returns whether anything actually moved, so an idle tick only forces a
    /// redraw when the numbers changed.
    pub(in crate::cli) fn update_cumulative_tokens(&mut self, cumulative: TurnTokens) -> bool {
        let meter = meter_cumulative(cumulative);
        let changed = self.token_usage.cumulative_tokens != meter.cumulative_tokens
            || self.token_usage.cumulative_prompt_tokens != meter.cumulative_prompt_tokens
            || self.token_usage.cumulative_cached_tokens != meter.cumulative_cached_tokens;
        self.token_usage.cumulative_tokens = meter.cumulative_tokens;
        self.token_usage.cumulative_prompt_tokens = meter.cumulative_prompt_tokens;
        self.token_usage.cumulative_cached_tokens = meter.cumulative_cached_tokens;
        changed
    }

    pub(in crate::cli) fn reset_token_usage(
        &mut self,
        session_tokens: u64,
        context_window: Option<usize>,
    ) {
        self.token_usage = render::TokenMeter {
            session_tokens,
            context_window,
            ..Default::default()
        };
    }

    pub(in crate::cli) fn update_thinking_variant(&mut self, variant: Option<&str>) {
        self.thinking = if self.mixed_models {
            None
        } else {
            variant.map(str::to_string)
        };
    }
}

pub(in crate::cli) fn repl_footer_line(
    mode: AgentMode,
    footer: &ReplFooterStatus,
    cols: usize,
) -> String {
    let cols = cols.max(1);
    let bar = input_prompt_bar(mode);
    let bar_width = visible_width(&bar);
    // The footer carries only the two standing gauges — how much context is
    // left, and what the session has cost. The per-turn figure is transient and
    // already has its own home in the `Token:` line printed after each reply;
    // keeping it here cost 14 columns and pushed the whole footer past 80.
    let usage = render::TokenMeter {
        turn_tokens: 0,
        ..footer.token_usage
    };
    // Narrow terminals: drop the cumulative total first, then the percent,
    // so the core context meter survives as long as possible.
    let mut right_plain = String::new();
    for (with_cumulative, with_percent) in [(true, true), (false, true), (false, false)] {
        let meter = render::TokenMeter {
            cumulative_tokens: usage.cumulative_tokens.filter(|_| with_cumulative),
            ..usage
        };
        right_plain = render::format_token_usage_inline_opts(&meter, with_percent);
        let left_room = cols
            .saturating_sub(bar_width)
            .saturating_sub(visible_width(&right_plain));
        if left_room >= 24 {
            break;
        }
    }
    let right = format!("\x1b[2m{right_plain}\x1b[0m");
    let right_width = visible_width(&right);
    let left_budget = cols.saturating_sub(bar_width.saturating_add(right_width).saturating_add(1));
    let left = repl_footer_left(mode, footer, left_budget);
    let gap = cols
        .saturating_sub(
            bar_width
                .saturating_add(visible_width(&left))
                .saturating_add(right_width),
        )
        .max(1);
    format!("{bar}{left}{}{right}", " ".repeat(gap))
}

pub(in crate::cli) fn repl_footer_left(
    mode: AgentMode,
    footer: &ReplFooterStatus,
    width: usize,
) -> String {
    let thinking = footer.thinking.as_deref().unwrap_or_default();
    let colored_thinking = (!thinking.is_empty()).then(|| primary_footer_text(thinking));
    let colored_thinking = colored_thinking.as_deref().unwrap_or_default();
    // 回合运行中,模型信息右侧是 Miyu 的声波律动(用户 08-20 选定):五柱
    // 波浪的高度与亮度随帧流动,颜色跟随模式主色(普通蓝/dev 酒红)。与
    // 模型信息之间隔三个空格,不进 " · " 序列(用户点名)。
    let wave = footer
        .running_spinner
        .map(|frame| sound_wave_frame(frame, mode == AgentMode::Dev));
    let with_wave = |text: String| match wave.as_deref() {
        Some(wave) => format!("{text}   {wave}"),
        None => text,
    };
    let provider = format!("\x1b[2m{}\x1b[0m", footer.provider);
    let mode = colored_footer_mode_label(mode);
    let full = with_wave(repl_footer_left_parts(
        &mode,
        &footer.model,
        Some(&provider),
        colored_thinking,
    ));
    if visible_width(&full) <= width {
        return full;
    }

    let compact = with_wave(repl_footer_left_parts(
        &mode,
        &footer.model,
        None,
        colored_thinking,
    ));
    if visible_width(&compact) <= width {
        return compact;
    }

    let fixed_width =
        visible_width(&mode)
            .saturating_add(3)
            .saturating_add(if thinking.is_empty() {
                0
            } else {
                3 + visible_width(colored_thinking)
            });
    let model_budget = width.saturating_sub(fixed_width).max(1);
    let model = truncate_display(&footer.model, model_budget);
    with_wave(repl_footer_left_parts(
        &mode,
        &model,
        None,
        colored_thinking,
    ))
}

pub(in crate::cli) fn repl_footer_left_parts(
    mode: &str,
    model: &str,
    provider: Option<&str>,
    thinking: &str,
) -> String {
    let mut endpoint = model.to_string();
    if let Some(provider) = provider.filter(|provider| !provider.is_empty()) {
        if !endpoint.is_empty() {
            endpoint.push(' ');
        }
        endpoint.push_str(provider);
    }
    let mut parts = vec![mode.to_string(), endpoint];
    if !thinking.is_empty() {
        parts.push(thinking.to_string());
    }
    parts.join(" · ")
}

/// 声波律动帧:五柱波浪,正弦驱动高度,三档颜色全部取自终端 16 色盘里由
/// matugen 绑定的语义色,不碰 bright 位——用户的 kitty 模板里 color12(94)
/// 是写死的 `#a39ec4`,不随壁纸换色(09-05 用户实录:波峰颜色对不上)。
/// 普通模式:峰=primary(34 加粗)、中=secondary(96)、谷=secondary_fixed_dim
/// (36 加 dim);dev 模式整条走 tertiary(35)的加粗/正常/dim 三档。
/// 每帧相位步进 0.24 rad,配合 80ms 的 footer tick 约每秒 3 rad,与演示稿
/// 的流速一致。
pub(in crate::cli) fn sound_wave_frame(frame: usize, dev: bool) -> String {
    const LEVELS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    let (hi, mid, low) = if dev {
        ("\x1b[1m\x1b[35m", "\x1b[35m", "\x1b[2m\x1b[35m")
    } else {
        ("\x1b[1m\x1b[34m", "\x1b[96m", "\x1b[2m\x1b[36m")
    };
    let t = frame as f32 * 0.24;
    let mut out = String::new();
    for i in 0..5 {
        let height = ((t - i as f32 * 0.9).sin() + 1.0) / 2.0;
        let glyph = LEVELS[((height * (LEVELS.len() - 1) as f32) as usize).min(LEVELS.len() - 1)];
        out.push_str(if height > 0.72 {
            hi
        } else if height > 0.35 {
            mid
        } else {
            low
        });
        out.push(glyph);
        out.push_str("\x1b[0m");
    }
    out
}

pub(in crate::cli) fn colored_footer_mode_label(mode: AgentMode) -> String {
    let label = mode.label();
    match mode {
        AgentMode::Normal => primary_footer_text(label),
        // tertiary(35 酒红,与 render/webui 的 tertiary 一致),区别于普通
        // 模式的 primary 蓝。
        AgentMode::Dev => format!("\x1b[1m\x1b[35m{label}\x1b[0m"),
    }
}

pub(in crate::cli) fn primary_footer_text(text: &str) -> String {
    format!("\x1b[1m\x1b[34m{text}\x1b[0m")
}

pub(in crate::cli) fn turn_meter(
    turn: TurnTokens,
    session_tokens: u64,
    context_window: Option<usize>,
    cumulative: TurnTokens,
) -> render::TokenMeter {
    render::TokenMeter {
        turn_tokens: turn.total,
        turn_prompt_tokens: turn.prompt,
        turn_cached_tokens: turn.cache_read,
        session_tokens,
        context_window,
        ..meter_cumulative(cumulative)
    }
}

/// The footer/status display must reflect the session's pinned model pool,
/// not just the global config.
pub(in crate::cli) fn footer_config_for_session(
    paths: &MiyuPaths,
    config: &AppConfig,
    session_id: &str,
) -> AppConfig {
    let mut config = config.clone();
    let Ok(store) = StateStore::new(paths) else {
        return config;
    };
    if let Ok(Some(models)) = store.session_model_override(session_id) {
        // 与 `apply_session_model_override` 同一道守卫:远端 REPL 走的是这条路,
        // 覆盖指向已删除的模型时曾让 `miyu normal` 整个起不来(08-28)。
        match config.usable_model_override(models) {
            Some(usable) => config.active_provider_models = Some(usable),
            None => crate::cli::model_cmds::drop_stale_model_override(&store, session_id),
        }
    }
    config
}
