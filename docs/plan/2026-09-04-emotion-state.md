# 情绪状态功能设计（2026-09-04）

状态：**设计稿，待拍板**。用户 09-04 拍板"情绪可以做"。这是功能提案，面板部分见 `2026-09-04-dashboard-design.md` §3.6。

## 0. 前提

Miyu 现在没有情绪状态。原型（AstrBot real_context 插件）的情绪是：全局单状态、二维（valence 心情 −1..1，arousal 表达欲 0..1）、**无 LLM 的固定增量启发式**（每次回复 +0.02，直接互动/主动接话再加一点，处理违规内容扣，回复超 120 字加表达欲）、按时间回归基线、早晚修正、无人互动冷清、影响三处（主动回复阈值、语气注入、主动消息门控）。

Miyu 不照搬的原因：① Miyu 已经有一条每次回复后跑的 LLM 关系维护任务（好感度更新），情绪增量可以搭这趟车拿到真实语义判断，不必只靠固定数；② Miyu 的判官已有一整套"程序调整项"槽位（续聊、直触、好感、热度、短消息），情绪就是再加一项，不另起炉灶；③ Miyu 没有"主动消息门控"这种东西（`proactive.rs` 只是直发文本），第三个影响点没有对应物；④ 状态得按人格分键，和好感度一致。

## 1. 模型

**状态**（每 (bot 账号, 人格) 一份，跨群全局）：

```rust
struct EmotionState {
    version: u32,            // 1
    valence: f64,            // -1..1,基线 0
    arousal: f64,            // 0..1,基线 0.5
    updated_at: i64,         // 上次写入,衰减以此为起点
    daily_date: String,
    daily_interactions: u64,
    last_interaction_at: i64,
    events: Vec<EmotionEvent>,   // 新在前,上限 100
}
struct EmotionEvent {
    delta_valence: f64, delta_arousal: f64,
    before: (f64, f64), after: (f64, f64),
    label_before: String, label_after: String,
    source: String,        // reply | llm | moderation | idle | manual
    reason: String,
    group_id: String, message_id: String,
    created_at: i64,
}
```

**存储**：`platform_plugin_kv`，scope = `{plugin_id:"real_context", platform:"onebot", account_id:<bot>, conversation_kind:"emotion", conversation_id:"*"}`，key = `emotion_state:<persona_scope>`。与好感度同一张表、同一套 `plugin_update_json` 读改写带 revision，并发安全。

**标签**（七态，zh/en）：由 (valence, arousal) 查表，顺序判定：烦躁 irritable（v≤−0.35 ∧ a≥0.45）→ 低落 down（v≤−0.35）→ 疲惫 tired（a≤0.25）→ 兴奋 excited（v≥0.35 ∧ a≥0.70）→ 调皮 playful（v≥0.25 ∧ a≥0.65）→ 愉快 cheerful（v≥0.30）→ 平静 calm。阈值进 `scoring.rs` 常量，不进配置。

**衰减**：读时惰性计算，不起定时器。指数回归：`v = v0 · 2^(−Δt/半衰期_v)`，`a = 0.5 + (a0−0.5) · 2^(−Δt/半衰期_a)`。默认半衰期 valence 6 h、arousal 45 min。只在有写入（回复后 touch）时把衰减后的值落盘，纯读不写。

**有效状态**（在存储态之上叠加，不落盘）：
- 时段：06–10 表达欲 +0.06；23–06 −0.12。
- 冷清：距最近一条**人类**消息（查 `message_history` 该账号最新 `is_bot=0` 的 `sent_at`）超过 `idle_loneliness_hours`(3) 后，valence −0.18·p，arousal −0.10·p，p = min(1, (idle−阈值)/(2·阈值))。

## 2. 增量来源

三层，从便宜到贵，都可独立开关：

**① 启发式（默认开，零成本）**：回复完成时按回合事实定增量：
| 事实 | Δv | Δa | 来源标签 |
|---|---|---|---|
| 普通回复 | +0.02 | +0.02 | reply |
| 直接互动（@/引用/私聊） | +0.02 | +0.015 | reply |
| 主动接话（active/keyword 触发） | +0.015 | +0.02 | reply |
| 判官违规判定为真（`ModerationResult.violation`） | 取 min(Δv−0.055, −0.025) | +0.02 | moderation |
| 回复被撤回（自己的消息被管理员撤） | −0.04 | −0.02 | moderation |
| 回复 ≥120 字 | 0 | +0.01 | reply |

**② LLM 语义增量（搭好感度更新的车）**：`affection_update_enable` 打开时，`build_update_prompt` 的返回 JSON 加一个字段 `"emotion":{"valence_delta":0,"arousal_delta":0,"reason":""}`，范围各 [−0.15, +0.10]，置信沿用同一个 `confidence`。同一次回复里 LLM 增量**替换**启发式增量（不叠加），来源标签 llm。好感度关掉时自动退回 ①。零额外调用。

**③ 手动**：面板设值 / 重置，来源 manual。

每日限幅：valence 日累计增益 ≤ +0.6、亏损 ≤ −1.0，防止刷。

## 3. 影响点

**A. 主动回复阈值**（`emotion_influence_threshold`，默认开）：
`factor = (v + (2a − 1)) / 2`，`adjust = clamp(−factor · max_adjust, ±max_adjust)`，`max_adjust` 默认 0.12。心情好、表达欲高 → 阈值降 → 更想接话。接入 `inject.rs:354-420` 的调整项组装，作为 `emotion_adjustment` 与好感/热度并列；`JudgeResult`、`ActiveReplyDecisionLog` 各加一个字段；判官提示词里"Current program adjustments"那行追加 `emotion {:+.3}`。

**B. 语气**（`emotion_influence_tone`，默认开）：在 `inject.rs:370-392` 注入好感度关系提示的同一位置，追加一行**陈述式**状态（遵守 prompt-hint-style：只说是什么，不下指令）：

```
<internal-state>心情：不错；精神：比较有精神。</internal-state>
```

不写数值、不写"请据此调整语气"。文案从 valence/arousal 五段文本表取（心情很好/不错/平稳/偏低/很差；很有表达欲/比较有精神/普通/有点犯困/很没精神）。

**C. 表情包自动发送概率**（可选，`emotion_influence_meme`，默认关）：`auto_send_probability × (0.5 + a)`，表达欲高多发图。只是把已有概率乘一个因子，无新逻辑。

**不做**：主动消息门控（Miyu 无对应功能）；让模型查询自己情绪的工具（提示注入已覆盖，工具会诱导它"谈论情绪系统"）。

## 4. 配置（`RealContextPluginSettings` 新增，TUI 加 `emotion.rs` 分组）

| 键 | 默认 | 说明 |
|---|---|---|
| `emotion_enable` | false | 与 `affection_enable` 同款默认关 |
| `emotion_heuristic_enable` | true | 层① |
| `emotion_llm_enrich_enable` | true | 层②，需好感度更新开着才生效 |
| `emotion_influence_threshold` | true | 影响 A |
| `emotion_max_threshold_adjust` | 0.12 | 0..1 |
| `emotion_influence_tone` | true | 影响 B |
| `emotion_influence_meme` | false | 影响 C |
| `emotion_valence_half_life_hours` | 6.0 | 0.1..168 |
| `emotion_arousal_half_life_minutes` | 45 | 1..10080 |
| `emotion_idle_loneliness_hours` | 3.0 | 0.1..168 |
| `emotion_morning_arousal_bonus` | 0.06 | 0..0.5 |
| `emotion_night_arousal_penalty` | 0.12 | 0..0.5 |
| `emotion_daily_valence_gain_limit` | 0.6 | |
| `emotion_daily_valence_loss_limit` | 1.0 | |

## 5. 代码落点

- `src/platforms/plugins/real_context/emotion/{mod,scoring,logging}.rs`：镜像 `affection/` 结构。`snapshot()` 给判官与注入用，`touch_after_reply()` 写增量，`apply_llm_delta()` 接层②。
- `affection/mod.rs`：`build_update_prompt` 输出加 `emotion` 字段；`apply_update` 里解析后转交 `emotion::apply_llm_delta`。
- `inject.rs`：判官请求加 `emotion_adjustment`；注入点加 `<internal-state>`。
- `judge.rs`：`JudgeRequest`/`JudgeResult` 各加字段；提示词调整行加一项。
- `decision_log.rs`：加 `emotion_adjustment` 行（只在 |值|≥0.0005 时打印，同现有惯例）。
- `config/platform_plugins/real_context.rs` + `config_tui/real_context/emotion.rs`。
- 面板：`src/web/dashboards/affection.rs` 里加 `/api/dash/affection/emotion/{state,events}` + `PUT state` + `POST reset`。

估算：核心 ~500 行 Rust + 测试 ~200 行 + TUI ~120 行 + 面板后端 ~120 行 + 前端 ~250 行。

## 6. 验收（按 verify-before-shipping）

1. 单测：标签查表边界、衰减公式（0/半衰期/∞）、日限幅、有效状态叠加。
2. 桩 LLM（stub-llm）跑 5 回合：启发式增量按表落库；打开好感度更新后 LLM 增量替换启发式，事件 source 正确。
3. 判官取证：同一条消息在 (v=0.6,a=0.8) 与 (v=−0.6,a=0.2) 下阈值差 ≈ 2·max_adjust，决策日志两行可对上。
4. 人格遵循 A/B（沿用 08-14/23 测具）：`<internal-state>` 注入不应把风格锁分数拉低；若掉分改位置不改措辞。
5. 冷清路径：把 `message_history` 最新人类消息时间拨到 5 h 前，`effective` 值应下压且不写库。

## 7. 待拍板

1. 情绪按 (账号, 人格) 全局，不按群。
2. 层②搭好感度更新的车而不是单独调用。
3. 影响 C（表情包概率）做不做。
4. 面板放在好感度面板的第二个标签（rail 名改"好感·情绪"）。
