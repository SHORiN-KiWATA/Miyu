//! 情绪状态设置(09-04):一个开关菜单 + 一张数值表单。

use crate::config_tui::*;

pub(in crate::config_tui) fn edit_real_context_emotion(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let options = [
            format!(
                "{}: {}",
                t("Emotion state", "情绪状态"),
                boolean_label(settings.emotion_enable)
            ),
            format!(
                "{}: {}",
                t("Heuristic deltas after replies", "回复后按回合事实加减"),
                boolean_label(settings.emotion_heuristic_enable)
            ),
            format!(
                "{}: {}",
                t(
                    "LLM deltas via affection update",
                    "搭好感度更新拿模型语义增量",
                ),
                boolean_label(settings.emotion_llm_enrich_enable)
            ),
            format!(
                "{}: {}",
                t("Adjust active-reply threshold", "影响主动回复阈值"),
                boolean_label(settings.emotion_influence_threshold)
            ),
            format!(
                "{}: {}",
                t("Hint tone at turn tail", "回合尾部注入语气提示"),
                boolean_label(settings.emotion_influence_tone)
            ),
            t("Decay and limits", "衰减与限制").to_string(),
        ];
        draw_menu(
            stdout,
            t(" EMOTION STATE ", " 情绪状态 "),
            &options,
            selected,
            "",
        )?;
        match read_key()? {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1).min(options.len() - 1),
            KeyCode::Enter => match selected {
                0 => {
                    settings.emotion_enable = select_bool(
                        stdout,
                        t("Emotion state", "情绪状态"),
                        settings.emotion_enable,
                    )?
                }
                1 => {
                    settings.emotion_heuristic_enable = select_bool(
                        stdout,
                        t("Heuristic deltas after replies", "回复后按回合事实加减"),
                        settings.emotion_heuristic_enable,
                    )?
                }
                2 => {
                    settings.emotion_llm_enrich_enable = select_bool(
                        stdout,
                        t(
                            "LLM deltas via affection update",
                            "搭好感度更新拿模型语义增量",
                        ),
                        settings.emotion_llm_enrich_enable,
                    )?
                }
                3 => {
                    settings.emotion_influence_threshold = select_bool(
                        stdout,
                        t("Adjust active-reply threshold", "影响主动回复阈值"),
                        settings.emotion_influence_threshold,
                    )?
                }
                4 => {
                    settings.emotion_influence_tone = select_bool(
                        stdout,
                        t("Hint tone at turn tail", "回合尾部注入语气提示"),
                        settings.emotion_influence_tone,
                    )?
                }
                5 => edit_real_context_emotion_values(stdout, settings)?,
                _ => {}
            },
            _ => {}
        }
    }
}

pub(in crate::config_tui) fn edit_real_context_emotion_values(
    stdout: &mut io::Stdout,
    settings: &mut RealContextPluginSettings,
) -> Result<()> {
    loop {
        let mut fields = vec![
            Field::new(
                t("Max threshold adjustment (0-1)", "阈值最大修正(0-1)"),
                settings.emotion_max_threshold_adjust.to_string(),
            ),
            Field::new(
                t("Mood half-life (hours)", "心情半衰期(小时)"),
                settings.emotion_valence_half_life_hours.to_string(),
            ),
            Field::new(
                t("Energy half-life (minutes)", "表达欲半衰期(分钟)"),
                settings.emotion_arousal_half_life_minutes.to_string(),
            ),
            Field::new(
                t(
                    "Loneliness after idle (hours)",
                    "无人互动多久开始冷清(小时)",
                ),
                settings.emotion_idle_loneliness_hours.to_string(),
            ),
            Field::new(
                t("Morning energy bonus", "早晨表达欲加成"),
                settings.emotion_morning_arousal_bonus.to_string(),
            ),
            Field::new(
                t("Night energy penalty", "深夜表达欲降低"),
                settings.emotion_night_arousal_penalty.to_string(),
            ),
            Field::new(
                t("Daily mood gain limit", "心情每日增益上限"),
                settings.emotion_daily_valence_gain_limit.to_string(),
            ),
            Field::new(
                t("Daily mood loss limit", "心情每日亏损上限"),
                settings.emotion_daily_valence_loss_limit.to_string(),
            ),
        ];
        if !run_form(
            stdout,
            t(" EMOTION DECAY AND LIMITS ", " 情绪衰减与限制 "),
            &mut fields,
        )? {
            return Ok(());
        }
        let mut candidate = settings.clone();
        let parsed = (|| -> std::result::Result<(), String> {
            candidate.emotion_max_threshold_adjust = real_context_value(&fields, 0)?;
            candidate.emotion_valence_half_life_hours = real_context_value(&fields, 1)?;
            candidate.emotion_arousal_half_life_minutes = real_context_value(&fields, 2)?;
            candidate.emotion_idle_loneliness_hours = real_context_value(&fields, 3)?;
            candidate.emotion_morning_arousal_bonus = real_context_value(&fields, 4)?;
            candidate.emotion_night_arousal_penalty = real_context_value(&fields, 5)?;
            candidate.emotion_daily_valence_gain_limit = real_context_value(&fields, 6)?;
            candidate.emotion_daily_valence_loss_limit = real_context_value(&fields, 7)?;
            candidate.validate().map_err(|error| error.to_string())
        })();
        match parsed {
            Ok(()) => {
                *settings = candidate;
                return Ok(());
            }
            Err(error) => message(stdout, &error)?,
        }
    }
}
