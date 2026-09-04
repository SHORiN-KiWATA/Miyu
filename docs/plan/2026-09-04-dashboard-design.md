# 控制台 Dashboard 设计（2026-09-04）

状态：**P0–P6 已落地（09-04），P7 待做**。基于 09-04 对 Miyu 各域真实实现的复核（不是 AstrBot 原型），原型只当功能启发。上一份分期方案见 `docs/plan/2026-09-03-plugin-dashboard-and-io.md`，本文覆盖其"一、插件 Dashboard 移植"部分。

---

## 0. 复核结论（先纠错）

| 疑问 | 结论 | 证据 |
|---|---|---|
| 赞助统计 | **Miyu 没有**，不做 | `src`/`web` 全文无 sponsor/赞助/donat |
| 情绪状态（valence/arousal） | **Miyu 没有**。全部分支 `src` 无 valence/arousal/mood 状态；`emotion` 只出现在表情包分类门 `emotion_or_meme`，`mood` 只在提示词里 | `src/tools/memes/validate.rs:112`、`real_context/targeting.rs:564` |
| 最接近"情绪"的东西 | ① 好感度分（每用户、账号级、按人格分键）② `reply_heat`（每会话内存值，重启即丢，是限流不是情绪）| `affection/mod.rs:34`、`real_context/runtime.rs:71` |
| 主动回复裁决日志 | **已落盘，但是文本**：tracing 写到 `~/.miyu/cache/logs/miyu.<日期>.log`，按天轮转只留 8 天，`miyu::qq` 默认 INFO 级，每条判断都在。是本地化多行文本不是结构化数据 | `src/logging.rs:32-46`、`inject.rs:462/487/560/578` |
| 记忆 demo 的"已归档"过滤 | 空集：代码从不写 `archived`，真实第三态是 `forgotten` | `schema.rs:546`、`dash-memory.js:45` |
| 记忆 stats 接口 | 调 `init()` → 会建库并跑一次遗忘衰减，浏览接口不该有副作用 | `write.rs:135`、`dashboards/memory.rs:93` |

**情绪：用户 09-04 拍板做**。功能提案单独成文 `docs/plan/2026-09-04-emotion-state.md`（按 (账号, 人格) 全局二维状态，增量搭好感度更新的 LLM 车，影响判官阈值与语气注入）。面板挂在好感度面板第二标签，见 §3.6。

---

## 1. 面板清单（定稿版）

rail 顺序：用量 · 记忆 · 知识库 · 表情包 · QQ 消息记录 · 群管 · 好感·情绪 · 设置

分享文件已有独立小面板，不重做、不迁移。研究报告 / 网页图片缓存 / QQ 收文件三个小域价值低，**不做**。

| # | 面板 | 作用域选择器 | 数据落点 | 新后端 | 新前端 | 备注 |
|---|---|---|---|---|---|---|
| 1 | 记忆 | 人格 | `personas/<p>/memory/memory.db` + `evicted_context.db` | 250 | 400 | demo 扩完整 |
| 2 | 知识库 | 无（单库） | `data/kb/` | 350 | 550 | 上传/树/搜索/预览/重建 |
| 3 | 表情包 | 库（默认=当前人格映射的库） | `data/memes/<lib>/index.json` + 内置库 | 400 | 600 | 需要图片路由 |
| 4 | 群聊 | 账号 + 会话 | `message_history/history.sqlite3` | 350 | 550 | 消息/统计/撤回三标签 |
| 5 | 群管 | 账号 + 群 | `platform_plugin_kv` 三键 | 150 | 400 | 从设置页搬出 |
| 6 | 好感·情绪 | 人格 | `platform_plugin_kv` `affection_profile:<p>` / `emotion_state:<p>` | 370 | 650 | 好感度写路径全新；情绪依赖功能先落地 |
| — | 定时消息 | 账号 | config 子树 | 80 | 200 | 可选，放群聊面板第四标签 |
| — | 主动回复裁决 | 账号 + 群 | 文本日志（8 天） | 120 | 300 | 需补结构化写入，见 §3.8 |

合计约 1900 行后端 + 3150 行前端（不含情绪功能本身）。

---

## 2. 统一样式规范

所有面板同一骨架，从上到下：

```
.con-head        标题 + 作用域选择器(dash-select) + 刷新(dash-icon-button)
.dash-cards      3~5 张统计卡(dash-card),数字 tabular-nums,hint 放次级数
.dash-toolbar    左:con-segmented 标签 / dash-chip 过滤  右:dash-search-box
主体             dash-table(表) | dash-gallery(图) | dash-tree(树) | dash-timeline(时间线)
.dash-pager      服务端分页;游标分页的面板显示"更早/更新"而不是页码
.dash-drawer     右侧抽屉:详情 dash-meta + 编辑表单 + 底部动作;危险动作走 dash-confirm
```

**新增零件（进 `dashboards.js` + `styles.css`）**：
- `dash-gallery`：auto-fill 网格，`minmax(140px,1fr)`，卡片含缩略图、名称、状态 chip；点开走抽屉不走 lightbox（lightbox 留给"看原图"按钮）。
- `dash-tree`：目录树，可折叠，文件行显示大小与索引状态点。
- `dash-timeline`：垂直时间线，左侧时间，右侧事件卡；动作类型用 chip 色。
- `dash-sparkline`：内联 SVG 折线（属性画，不写 style，CSP 允许），用于好感度分数曲线。
- `dash-scope`：作用域选择器统一组件，位置固定在 `.con-head` 右侧；人格型/账号型/库型三种，都记住上次选择（localStorage，try/catch）。
- 条形图与热力：复用用量页 `.u-bars` / `.u-heatmap`，不新写。

**颜色只用别名层**：`--surface-1/2/3`、`--text/-soft/-faint`、`--accent`、`--danger`、`--line`、`--chart-N`、`--heat-N`。状态 chip 统一词表：`is-active`（正常）、`is-muted`（禁用/遗忘/过期）、`is-warn`（待处理/待晋升/陈旧）、`is-danger`（违规/踢黑）、`is-builtin`（内置/只读）。

**行为规范**：
- 读 `require_auth`，写 `require_mutation`；同步存储全部 `spawn_blocking`。
- 大列表服务端分页，`limit` 夹 1..500；消息记录用游标（`sent_at,row_id`）不用 offset。
- 浏览接口零副作用：不建库、不触发衰减、不重建索引。
- 长任务（KB 重建、默认库更新）不轮询 2s；给一个 `POST .../reindex` 起任务 + `GET .../status`，前端 5s 轮询到结束或订阅 SSE。
- 搜索防抖 250ms；空态统一 `dash-empty` 文案"暂无 X"。
- 图片一律新路由按 `(域, id)` 解析路径，绝不接受调用方传路径。

---

## 3. 逐面板设计

### 3.1 记忆（扩完整）

**作用域**：人格下拉（已有 `/personas`）。

**标签**：事实 · 经历 · 归档回合。

- 事实：列内容 / 类型（`memory_type` 六种）/ 真值（`truth_status` 五种）/ 重要度 1-5 / 置信 / 强度 / 召回 / 更新时间。过滤：状态（active/forgotten，**删掉 archived/pending**）、类型、真值、可见性、标签、主体。
- 经历：列内容 / 保留（短期·长期）/ 整理状态（未整理·已整理·待晋升·已晋升）/ 来源（本地·平台+群）/ 强度 / 到期倒计时。过滤同上 + 保留 + 来源平台。
- 归档回合：FTS 搜索（`search_evicted_context_filtered` 已有），列时间 / 角色 / 片段 / 分数；时间范围过滤；显示 embedding 覆盖率（`evicted_embeddings/evicted_turns`）。

**统计卡**：事实、经历（短期/长期）、待整理 + 待晋升、待处理事件、归档回合 + 覆盖率。**不要用 `stats()`**，改 browse 式只读查询（不存在按空处理）。

**抽屉**：全字段 `dash-meta`（含 origin_* 七个来源字段、tags、source_episode_ids）；**修订历史**时间线（`memory_revisions` 有写无读，直接查）；关联经历（按 `source_episode_ids` 反查）。动作：编辑内容（新写路径：改 content + 写一条 revision，`updated_at` 刷新）、改重要度/真值/标签、删除。

**顶部动作**：手工新增事实（`remember_fact`，需要 `set_request_context` 给归属）、清空待处理事件、清空归档回合、重置本人格记忆（已有 `/reset-memory`，二次确认要求输入人格名）。

**不做**：唤醒整理器（daemon 私有 handle 未暴露，另开票）；`skill_records`（死表）。

### 3.2 知识库

**布局**：左 `dash-tree`（1/3 宽）+ 右主区（预览 / 搜索结果切换）。

- 树：从 `files.name` 按 `/` 拆；`default-kb/` 前缀单独成"内置库"组，标只读 chip，组头显示 `DefaultKbState`（远端提交、是否有更新、上次导入）与"更新"按钮。文件行：大小、索引点（绿=chunk 的 sha 与当前一致；黄=陈旧；灰=无 chunk）。
- 预览：`read_file_readonly` 分页读（默认 200 行，"继续"加载），等宽字体，行号。
- 搜索：`search_readonly`，结果卡显示 path / 分数 / 来源 chip（keyword·semantic）/ 片段高亮；顶部一行"本次是否用到语义"（`semantic_used`）。另有"按文件名"模式走 `find_by_name_readonly`，显示 `match_reason`。

**统计卡**：文件数、总大小、语义块数、嵌入模型（provider/model 或"未启用"）、陈旧文件数、重建状态（`embedding.lock` 存在=进行中）。

**上传**：拖拽或选择，支持 `webkitdirectory` 整目录；前端按 allowlist（扩展名 + 裸文件名）与 1 MB 预检并标红跳过项；后端流式落临时文件 → `import_file(path, name)`（需把可见性从 `pub(in crate::tools)` 放宽到 `crate::web`），**保留 `reject_non_kb_upload` 守卫**；目录上传完成后触发一次重建。

**动作**：删除文件、重命名（=读+写+删，二期）、重建嵌入（`POST /reindex` 复用 `spawn_embedding_reindex` 子进程；`GET /status` 返回 lock 存在与 chunk 计数；lock 陈旧超 1h 给"清理锁"按钮）、更新内置库（`default_kb::update` 有 `UpdateStage` 回调，包成任务把阶段流出去）。

**说明给用户**：只收 UTF-8 文本，不收 PDF/图片；关键词搜索是全量扫描无索引，库大了会慢，这是现状不是面板问题。

### 3.3 表情包

**作用域**：库下拉。选项 = 磁盘上 `data/memes/*` ∪ 内置库名；默认选中当前人格映射的库；下拉旁一行小字"人格 X → 库 Y"。人格作用域名含 `-md` 后缀，显示时不要裸露，用 `persona_libraries` 映射的原名。

**画廊**：`dash-gallery`，缩略图走新路由 `GET /api/dash/memes/image?library=&id=`（按 `LoadedMeme.path` 解析，`stream_file_response` 加图片 MIME），GIF 显示静态首帧（`validate.rs:238` 已有）并角标"动图"。卡片底部：中文名 + 状态 chip。

**四种状态**（不是开关）：内置 / 用户 / 已覆盖（用户影子盖住内置）/ 已禁用。列表必须用 `find_meme_any` 路线才能列出禁用项，`load_library` 会把它们滤掉。

**过滤**：状态、动图、来源（手工/QQ 收集）、标签 chip 云、搜索框用 `score_meme` 同一套打分，并标注"模型看到的前 3 名"。

**统计卡**：总数（内置/用户）、禁用数、QQ 收集数、最近 7 天收集数、库索引更新时间。

**抽屉**：大图、全字段、origin（平台/群/发送者/发送时间/收集时间）、使用记录（`platform_meme_refs` 按 `(library, meme_id)` 统计入站/出站次数，新查询）。编辑：中英文名、描述、用法、标签（**补 `update` 缺失的长度/唯一校验**）。动作：启用/禁用、重新 AI 分类（`classify_meme_image`）、删除（回收站 / 彻底；内置项只能禁用，按钮文案要写明）。

**上传**：浏览器文件 → 临时文件 → `add_meme`。两种模式：AI 分类（可能返回 `rejected` 或 `needs_user_info`，前端把原因显示出来并允许切到手填）/ 手填元数据（中文名、描述、用法必填）。上传前用 `validate.rs` 的包络（格式、32..4096 px、≤16 MP、GIF ≤120 帧 15 s、≤max_image_mb）在前端预检。上传队列逐张显示结果。

**设置区**（抽屉或底部卡）：`enabled`、`persona_libraries`、`max_image_mb`、`search_max_results`、`auto_send_*`；QQ 收集器 `collect_probability` / `max_images_per_message` / `allow_non_admin_save_tool`。**不显示** `allow_gif_animation`（死配置）和 `width/height_percent`（终端专用）。写 `PUT /api/config` 只补丁这两个子树，不整份序列化。

**并发**：收集器会在面板打开期间改库，抽屉保存前比对 `index.json` mtime，变了提示刷新；所有写走 `library_lock`。

### 3.4 群聊（消息历史）

**作用域**：账号 + 会话。会话列表需要新查询 `SELECT conversation_kind, conversation_id, COUNT(*), MAX(sent_at)`；群显示群号（有群名缓存就带上）。

**标签**：消息 · 统计 · 撤回 ·（可选）定时消息。

- 消息：时间 / 发送者（名 + id，bot 行标 chip）/ 正文 / 媒体占位 chip（`media_json` 的 kind：图片·表情·文件·语音·视频，**没有图片字节也没有路径，只显示 chip**）/ 引用 / 撤回标记。过滤：发送者、时间范围、仅撤回、仅含媒体。搜索：≥3 字走 FTS trigram，否则 LIKE，前端提示"少于 3 字为模糊匹配"。游标分页。
- 统计：时间窗 segmented（7/30/90/全部）；`activity_ranking` 已有 → 领奖台前三 + 条形图（`.u-bars`）+ 表（消息数 / 活跃天 / 首末发言）；新增"按小时 × 星期"热力（`.u-heatmap` 7 行 24 列，新查询）；bot/人类占比；媒体类型占比（解析 `media_json`）。
- 撤回：`recalls` 表，谁撤了什么、何时，能对上原文就显示。

**统计卡**：消息总数、会话数、DB 文件大小、最早消息日期、撤回数。**没有自动保留策略**，卡片直说。

**动作**：删除历史（模式：全部 / 保留 N 天 / 按发送者 / 按时间段；复用 `delete_history`；工具侧的 `live_admin_message` 门在 web 上无法复现，用 `require_mutation` + 输入会话 id 确认代替）；重置上下文边界（`reset_context`，显示当前边界）。

### 3.5 群管

**作用域**：账号（`connected_accounts`）+ 群。群列表需要新查询：`SELECT DISTINCT account_id, conversation_id FROM platform_plugin_kv WHERE plugin_id='qq_group_management'`。

**标签**：时间线 · 违规者 · 踢人。

- 时间线：`load_all_events` 合并三键去重 → `dash-timeline`。动作 chip 六种（ban/unban/kick/kick_black/title_set/title_clear），来源标签（Miyu 工具 / 外部管理员通知 / 旧记录），禁言事件带派生状态（进行中/已过期/已解除/被覆盖，`ban_statuses`）。**这是新端点**，`management_events` 目前无 HTTP。
- 违规者：榜单（次数 / 累计时长 / 首末次 / 最近理由），抽屉里 `reason_history` 时间线（当前 UI 完全没渲染）。
- 踢人：含踢黑标记与操作者。

**统计卡**：bot 在群身份（`bot_role`）、违规者数 / 500 上限、踢人数、本月动作数。**不显示** `expired_record_retention_seconds` 等三个无人读的死设置。

**动作**：删除单个违规者（已有）、清空违规者 / 踢人（已有）、清空时间线（新）。**不提供**从 web 禁言/踢人/改头衔：这些是带在场消息门的 LLM 工具，绕过门就是另一个功能。

**迁移**：设置页 `qq-history-tool` 段落删掉，只留链接跳到本面板。

### 3.6 好感·情绪

两个标签：好感度 · 情绪。作用域共用人格下拉；情绪额外需要账号下拉（状态按 (账号, 人格) 分键）。

#### 好感度标签

**作用域**：人格（键是 `affection_profile:<persona>`）。用户列表需要新查询：`SELECT conversation_id, value_json FROM platform_plugin_kv WHERE plugin_id='real_context' AND conversation_kind='affection' AND key=?`。默认人格额外并入裸键 `affection_profile`。

**列表**：名字 / QQ / 分数 + 等级 chip（七级）/ 标签 / 消息数·直接互动·回复数 / 最近互动 / 自动更新开关。排序：分数、最近互动、消息数。过滤：等级、有无标签、自动更新关闭。

**统计卡**：档案数、等级分布（七段小条）、今日增/减预算使用（`daily_gain` / `affection_daily_gain_limit`）、自动更新总开关（`affection_enable` 默认 false，关着就把面板顶部横幅提示"功能未启用"）。

**抽屉**：`dash-sparkline` 分数曲线（`events` ≤50 点，score_before→score_after）；事件审计表（时间 / Δ / 置信 / 理由 / 标签增减 / message_id，比 LLM 工具看到的多出全部数字）；备注；计数器；普通用户 94 分上限提示（`affection_unlimited_user_ids` 之外）。

**动作（全部新）**：设分数、改备注、加减标签、切自动更新、清空事件、删除档案。全走 `plugin_update_json` 读改写带 revision，不覆盖收集器并发写。

#### 情绪标签

依赖 `2026-09-04-emotion-state.md` 落地后才有数据。功能未开（`emotion_enable=false`）时顶部横幅提示，下面仍显示空态骨架。

- **状态区**：一张大卡，左侧二维方格图（横轴心情 −1..1，纵轴表达欲 0..1，七个标签区域用 `--surface-2/3` 分块，当前点用 `--accent`，存储态用空心点、有效态用实心点），右侧四行：标签（存储 / 有效）、心情文本、精神文本、当前阈值修正 ±；下面两条 `dash-meter`：valence 与 arousal 各一条带基线刻度，hint 显示"距基线回归还需 X"（按半衰期算）。
- **修正项卡**：时段修正、冷清修正（距最近人类消息 X 小时）、今日互动次数、今日增益/亏损 vs 限幅。
- **曲线**：`dash-sparkline` 两条（valence / arousal），取 events ≤100 点，横轴时间。
- **事件表**：时间 / 来源 chip（reply · llm · moderation · idle · manual）/ Δv / Δa / 前→后标签 / 理由 / 群 / message_id。
- **动作**：手动设值（两个滑杆 + 理由必填，来源 manual）、重置到基线、清空事件。都走 `plugin_update_json`。

后端：`/api/dash/affection/emotion/state?persona=&account=`、`/events`、`PUT state`、`POST reset`，与好感度同一文件。

### 3.7 定时消息（可选，群聊面板第四标签）

任务表：会话 / 时间 / 星期 / 消息预览 / 指定账号 / 下次触发（用 `due_fires` 同款算法算）。CRUD 补丁 `platforms.qq.plugins.qq_scheduled_messages.settings.tasks`，校验镜像 `parse_task`。**没有触发历史**（进程内 2 天去重集），要看历史得先加落盘。不做"立即发送"。

### 3.8 主动回复裁决（群聊面板第五标签，需先补结构化写入）

**现状**：每条判断已经写进 `~/.miyu/cache/logs/miyu.<日期>.log`（`src/logging.rs`：按天轮转、留 8 个文件、`miyu::qq` 默认 INFO），本机 09-01/03/04 的日志里都能 grep 到"【主动回复判断：回复】"整段。所以"记录"这个目的已经满足。

**面板不能直接读它的原因**：① 内容按 locale 本地化（中文/英文两套字段名），② 多行自由文本，格式随代码演进，③ 只留 8 天，④ 和其他 126 处 `miyu::qq` 日志混在一起。写解析器会很脆。

**建议**：在 `inject.rs` 四个打日志的调用点旁边，**同时**把同一个 `ActiveReplyDecisionLog` 序列化成一行 JSON 写进 `history.sqlite3` 新表 `decisions`（账号、群、消息 id、发送者、触发类型、raw/final/threshold、五项调整 + 情绪调整、是否回复、模型判断、违规判定、端点、理由、时间），保留 30 天，索引 `(account, group, created_at)`。文本日志照旧不动。约 120 行。

**面板**：过滤（群 / 结果 / 触发类型 / 只看违规判定 / 时间范围），表列时间 / 发送者 / 消息摘要 / 触发 / raw→final vs 阈值 / 结果 chip / 模型；抽屉展开完整分数分解（条形叠加图：raw + 各调整项 = final，阈值一条竖线）、理由、违规判定详情。统计卡：24h 判断数、回复率、平均分、违规判定数。跳过名单编辑器（`apply_active_judgement_skip_editor_changes` 现成）放本标签底部。

价值：08-29 元层拒答那类取证现在靠手工 grep 日志，有这张表能直接按群按时间段拉出分数分布。

---

## 4. 公共后端工作（P0）

1. `assets.rs` 静态表重构：`(路径, include_str!, mime)` 一张表 + 通用 handler，之后加面板 JS 只改一行。09-03 未做。
2. 图片直出路由骨架：`stream_file_response` 已有 Range/nosniff，加图片 MIME 白名单；各域自己做 `(id) → path` 解析。
3. `platform_plugin_kv` 作用域枚举查询（群管 / 好感度 / 会话列表共用）。
4. 上传落临时文件的通用助手（分享文件已有流式写盘，抽出来）。
5. 轻量任务壳：`POST` 起任务 + `GET status`，KB 重建 / 默认库更新 / 大批量删除共用；不进 jobs 注册表（那是会话绑定的）。

## 5. 分期

| 期 | 内容 | 验收 |
|---|---|---|
| P0 | §4 五项 + `dashboards.js` 新零件（gallery/tree/timeline/sparkline/scope）| 记忆 demo 换新骨架跑通 |
| P1 | 记忆扩完整（修 archived、去 stats 副作用、修订历史、编辑）| 人格切换 + 编辑写 revision 实测 |
| P2 | 知识库 | 目录上传 + 陈旧检测 + 重建状态实测 |
| P3 | 表情包 | 四状态 + 上传两模式 + 收集器并发实测 |
| P4 | 群聊 + 群管（含设置页迁移）| 游标分页 + 时间线合并端点实测 |
| P5 | 好感度标签 | 写路径 revision 冲突实测 |
| P6 | 情绪功能（另一份文档）→ 情绪标签 | 单测 + 接口实测已过；桩 LLM 五回合未做 |
| P7 | 裁决结构化写入 → 裁决标签；定时消息标签 | **未做** |

每期独立可合并。

## 6. 已拍板 / 待拍板

已拍板（09-04）：情绪做；分享文件面板不动；研究报告/图片缓存/QQ 收文件不做。

待拍板：
1. 群管从设置页搬走（推荐）。
2. 裁决日志：是否接受"文本日志保留 + 另加结构化表"的方案；不接受就不做裁决标签。
3. 情绪功能提案里的四条（作用域、搭车、表情包概率、面板位置）。
4. 分期顺序 P1→P7。

---

## 调研来源

09-04 四路复核（记忆+KB / 表情包 / QQ 三域 / 文件与任务），关键坐标：
- 记忆 schema `src/memory/schema.rs:11-309`；browse `src/memory/browse.rs:46-124`；修订 `write.rs:440-451`；stats 副作用 `write.rs:135`
- KB `src/tools/knowledge_base/{store.rs:47-62, files.rs:349-372, index.rs:24-50, 268-297}`；default_kb `src/default_kb.rs:23-51`
- 表情包 `src/tools/memes/{library.rs:15-65, 104-200, crud.rs:57-438, validate.rs:10-236}`；收集器 `src/platforms/plugins/meme_collector.rs:75-287`；`platform_meme_refs` `src/state/migrations/baseline.rs:312-331`
- 群管 `src/platforms/plugins/group_management/records.rs:12-89, 213, 288, 433`；现有 API `src/web/qq_history.rs:32-160`；设置页 UI `web/app.js:2311-2431`
- 消息历史 `src/platforms/plugins/message_history/store/{schema.rs:7-306, query.rs:208-360}`；删除门 `tools/delete.rs:87`
- 好感度 `src/platforms/plugins/real_context/affection/{mod.rs:34-95, 819-833, scoring.rs:41-133}`；默认值 `src/config/platform_plugins/real_context.rs:178-204`
- 裁决日志 `src/platforms/plugins/real_context/decision_log.rs:8-33`；跳过名单 `active_judgement_skip.rs:128-192`；定时消息 `scheduled_messages/{mod.rs:41-112, schedule.rs:6-80}`
- 研究报告 `src/tools/deep_research/report.rs:174-207`；网页图片 `src/tools/web_images/download.rs:370-374`；文件路由 `src/web/{shared_files.rs, attachments.rs:33-168, assets.rs:450-553}`
- 面板接线（4 处手工注册）`web/index.html:19-21, 272-275, 351-353`、`src/web/{mod.rs:138, assets.rs:85-174, server.rs:313-321}`、`web/app.js:9867-9885`
