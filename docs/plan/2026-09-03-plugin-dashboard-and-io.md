# 插件 Dashboard 移植 + I/O 优化 方案（2026-09-03）

状态：**记忆浏览器 demo 已落地待验收（09-03 晚）；其余分期与 I/O 第一批待拍板**。本文只记结论与方案，取证细节见文末"调研来源"。

Demo 落点：`web/dashboards.js`（共享层：注册表 / api / 统计卡 / 分页 / 抽屉 / 确认框）、`web/dash-memory.js`、`src/web/dashboards/memory.rs`（`/api/dash/memory/{personas,stats,items}` + DELETE）、`src/memory/browse.rs`。控制台 rail 新增「记忆」。`assets.rs` 静态表重构（P0 第一项）**未做**，本次仍按 4 处手工注册。

---

## 一、插件 Dashboard 移植

### 1. 原型是什么

AstrBot 侧 8 个插件各带一个 `pages/dashboard/`，通过 `register_web_api` 挂后端路由、iframe + postMessage 桥接主站。盘点结果：

| 原插件 | 功能数 | 端点数 | 前端行数 | 复杂度 |
|---|---|---|---|---|
| mixed_knowledge_base | 12 | 10 | 2069 | 5（其中"知识宇宙"星图 ~1150 行纯特效） |
| real_context | 24 | 21 | 1910 | 5（五标签：聊天记录/发言统计/好感度/赞助/情绪） |
| qq_group_manage_with_log | 14 | 8 | 1707 | 4 |
| file_manager | 8 | 5 | 999 | 3 |
| emoji_pocket | 16 | 13 | 914 | 4（唯一在页面里改运行配置的） |
| persona_memory | 15 | 14 | 812 | 4 |
| deep_thinking | 9 | 10 | 716 | 3 |
| web_search（图片缓存） | 10 | 8 | 437 | 2 |

共 108 项功能 / 89 个端点 / 9564 行前端。**技术栈全部同构**：原生 JS、零框架、零图表库（手写 CSS 条形图）、零 WebSocket/SSE（只有三个页面在重建索引时 2s 轮询）。90% 的页面是同一套模板：统计卡 → 筛选条 → 列表/画廊 → 分页 → 详情抽屉/弹窗 → toast + confirm。8 份代码里 `esc/toast/confirmAction/api()` 逐字重复。

**不能照搬的三个原因**：① Miyu 的数据落点与 AstrBot 完全不同（见下表）；② Miyu WebUI 是 MD3 令牌体系、深色分层 surface，与 AstrBot 观感无关；③ AstrBot 的 iframe/asset_token/base64 图片全是为跨域隔离设计的，Miyu 同源直出用不着。

### 2. Miyu 侧对应关系（决定每个面板"有没有数据可展示"）

| 原面板 | Miyu 数据落点 | 已有 /api | 结论 |
|---|---|---|---|
| persona_memory | `data/personas/<p>/memory/memory.db`（facts/episodes/memory_revisions）+ evicted_context.db（FTS5） | 只有 `POST /api/memory/reset` | **纯 UI 工作，价值最高**（09-03 调研 §5.3 已点名） |
| mixed_knowledge_base | `data/kb/`：kb_meta.db（files 表）+ semantic_index.db + files/ | 无 | 数据齐；星图不做 |
| emoji_pocket | 文件系统 + 各库 `index.json`（内置库 + `data/memes/<lib>/`），配置 `plugins.memes`（含按人格映射） | 无 | 数据齐；"上传前 AI 预分析"需接 vision |
| qq_group_manage | SQLite `platform_plugin_kv`（offender_history / kick_history） | **已有 3 条**（qq_history.rs）+ 设置页内嵌 UI | 扩成完整三表 + 编辑 |
| real_context·聊天记录/发言统计 | `data/platforms/onebot/message_history/history.sqlite3` | 无 | 数据齐，统计是纯查询 |
| real_context·好感度 | `platform_plugin_kv` 的 `affection_profile:*` | 无 | 数据齐 |
| real_context·赞助/情绪 | **Miyu 无对应存储** | — | 不做，除非确认有需求 |
| file_manager | 分享文件（`shared_files` 表 + `data/shared/`）；QQ 收到的文件在 `cache/platform_files/qq/` | 已有 list/download/delete，**09-03 已补上传** | 分享面板已覆盖主功能；QQ 收文件浏览可选 |
| deep_thinking | `data/documents/deep-thinking/` 报告文件 | 无 | 列表 + 预览即可，小 |
| web_search 图片缓存 | `pictures/web-images/` 目录 | 无 | 最低优先级 |

### 3. 架构方案

**A. 挂载位置：控制台整页视图 + 左侧 rail**（`index.html` `.con-rail-item[data-console-panel]` + `.con-panel`，`app.js:9876 setConsolePanel` 懒加载分支）。现有"用量/设置"就是这个模式，新增一个面板 = HTML 加一个 rail 按钮和一个 panel div，JS 加一个懒加载分支。**不走 iframe，不做插件页面发现机制**。

**B. 前端拆文件，不往 app.js（10785 行）里塞**。照 `web/shared.js` 的 `window.MiyuXxx = (() => {...})()` 模式：
- `web/dashboards.js`：共享层——面板注册表、`api()` 封装（统一 `{ok,...}` 约定）、统计卡、筛选条、分页器、抽屉、confirm、toast 复用、lucide 图标小表。
- `web/dash-memory.js`、`web/dash-kb.js`、`web/dash-memes.js`、`web/dash-groups.js`、`web/dash-chat.js`、`web/dash-reports.js`：每域一文件，各 300-600 行。
- 每新增一个 JS 现在要改 4 处（`web/` 文件 → `mod.rs include_str!` → `assets.rs` handler + 版本化 replace → `server.rs` route）。**先做一次小重构**：`assets.rs` 改成一张 `(路径, include_str!)` 静态表 + 一个通用 handler，之后加文件只改一行。约 60 行。

**C. 后端每域一个文件**：`src/web/dashboards/{memory,kb,memes,groups,chat,reports}.rs`，各 150-400 行，路由前缀 `/api/dash/<domain>/...`，读用 `require_auth`、写用 `require_mutation`，照 `qq_history.rs` 的输入校验 + scope 构造写法。**不给 `PlatformPlugin` trait 加 web 钩子**：8 个面板里只有群管/聊天记录/好感度属于平台插件，其余是工具域数据；为三个面板加一层 trait 抽象不划算，且与现有 qq_history 的硬编码方式一致。

**D. 视觉重做**：只用 `styles.css` 的别名层变量（`--surface-1/2/3`、`--text/-soft/-faint`、`--accent`、`--danger`、`--line`），不写死色值，matugen 主题覆盖才能跟随。统一页面模板：顶部统计卡 grid（复用用量页的卡片样式）→ 筛选条（chip + 搜索框）→ 表格/画廊（`overflow-x:auto`）→ 底部分页 → 右侧抽屉编辑。图片画廊复用 `lightbox.js`。无图表库，条形图用 CSS。

**E. 约束**：
- CSP 是 `script-src 'self'; style-src 'self'`，禁内联脚本/样式。
- 大列表必须服务端分页（记忆库可能上万条，群聊历史更多）。
- 记忆库按人格分库 → 面板顶部要有人格选择器（默认当前人格）。
- KB 语义重建是长任务 → 走 jobs（已有 `/api/jobs`）而不是 2s 轮询。
- 设置面板"每次按键序列化整份 config"的写法（调研 W3）不要沿用。

### 4. 分期与估算

| 期 | 内容 | 后端 | 前端 | 备注 |
|---|---|---|---|---|
| P0 | `assets.rs` 静态表重构 + `dashboards.js` 共享层 + rail 入口 | 60 | 400 | 基建，之后每面板独立 |
| P1 | **记忆浏览器**：列表/搜索（关键词+类型+置信度）/单条编辑删除/修订历史/批量/导出 JSON/重建索引 | 350 | 550 | 价值最高，纯 UI |
| P2 | **知识库**：文件树/正文搜索/拖拽上传（含文件夹）/预览/删除/重建索引 | 300 | 500 | 复用 09-03 的流式上传 |
| P3 | **表情包**：画廊/筛选/上传队列（AI 预分析）/详情编辑/启停/按库切换/设置 | 350 | 550 | 唯一改运行配置的面板 |
| P4 | **群管**扩完整：禁言记录/违规者/踢人三表 + 编辑弹窗；**聊天记录 + 发言统计**（含领奖台/条形图）；**好感度**档案浏览编辑 | 450 | 700 | real_context 拆成三块 |
| P5 | 深度研究报告列表/预览；网络图片缓存画廊 | 120 | 250 | 可选 |

合计约 1600 行后端 + 3000 行前端（原型 9564 行的三分之一）。每期独立可验收、可合并。

**需要拍板**：① 分期顺序是否同意（推荐 P0→P1→P2→P3→P4）；② 赞助/情绪两个面板确认不做；③ 群管面板是从设置页里搬到控制台还是两处都留（推荐搬）。

---

## 二、I/O 优化

### 1. 已排除（不重提）

mimalloc / AppConfig→Arc / 资源外置 / panic=abort（low-footprint）；P5 缓存校验拆守卫、P1 seq 做 memo 键、P6 共享单连接（09-03 调研 §6 已推翻）；SSE 逐事件 flush（实际是 16 KiB/80ms 批合，别再提）；cache-usage jsonl 写法（句柄常驻，正确）；request_log（默认关，取证工具）；事务里做网络调用（`!Send` 结构上不可能）。

### 2. 候选清单（三档）

**一档：确定收益、零语义、可先做**

| # | 问题 | 位置 | 修法 | 量尺 |
|---|---|---|---|---|
| 1 | `AppConfig::load()` 无条件整读+解析 4.4 MB `models_cache.json`；daemon 启动走 `try_load` 全量，之后 `ensure_active_metadata` 见已加载直接跳过 → 低占用专项的裁剪与 trim 被旁路 | `config/io.rs:36` → `models_cache/mod.rs:108-129` | `try_load` 加 `is_loaded()` 短路 + mtime 复用 | 冷启动 wall time + daemon RSS（`testkit/low-footprint`） |
| 2 | `job_wake` 为读一个 bool 整份重载配置（顺带触发 #1） | `web/actor/job_wake.rs:152` | 改读 `state.manager.config` | 后台任务完成耗时 |
| 3 | KB `semantic_search` 全表扫 + 逐行 JSON 解 embedding，**未包 spawn_blocking**；同文件 `keyword_search` 08-18 已包 | `tools/knowledge_base/index.rs:171-215` | 先包 spawn_blocking；第二步 embedding 改 BLOB | 单 worker + 5ms 心跳探针（08-18 范式） |
| 4 | KB 两个连接一条 PRAGMA 都不设（无 busy_timeout → 撞锁即 SQLITE_BUSY；无 WAL） | `index.rs:315-328` | 补 busy_timeout=5000 + WAL + synchronous=NORMAL | busy 半是正确性；WAL 量 reindex 吞吐 |
| 5 | `usage.json` 每次 LLM 请求一次 `sync_all`（主请求 + 每个辅助请求各一次；10 轮工具回合 = 10 次 fsync）；且无目录 fsync，持久性本就是半套 | `state/usage.rs:23-30` | 删 sync_all 保留原子 rename，或降到回合末一次 | stub-llm 5 回合 wall time / iowait |
| 6 | `checkpoint_tool_flow` 每 LLM round 从头重建全量 tool_flow + 全列 JSON 覆写 → O(R²) | `agent/turn_loop/mod.rs:963-1081` → `tool_report.rs:362-410` → `turns.rs:177-184` | 增量构建；`prune_tool_output` 加字节级快筛 | scaling probe 8/16/32/64 轮看倍率，旧实现当预言机逐字节差分 |
| 7 | `store_replay_journal` 在 `BEGIN IMMEDIATE` 事务里逐条 remove(0) + 全量重序列化 → O(N²)，且持有全局连接锁 | `conversation_db/rows.rs:216-225` | 从尾部反向累加定保留点，一次 drain + 一次序列化 | 同上 |
| 8 | `recover_journal_assets` 为找一个 turn 全量 `load_turns`（调研 P9 未修） | `state/assets.rs:247-256` | 主键单行查询 | — |

**二档：需先量测（有语义或结构风险）**

| # | 问题 | 位置 | 备注 |
|---|---|---|---|
| 9 | `prepare_cached` 全仓零命中（conversation_db 65 处 `.prepare`） | `history.rs:126-145` 等 | 只换静态 SQL；`rows.rs:568` 动态 900 占位符的不能进缓存。单条 prepare 几十 µs，值不值要量 |
| 10 | 每回合 5-8 次 `effective_context_tokens` 全量重放（P1 已在册） | `agent/setup.rs:379-386` | `total_changes()` 写世代仍可走 |
| 11 | 每 LLM **round**（不是每 turn）重扫脚本目录 + 技能指纹（blake3 23 KB） | `turn_loop/mod.rs:76-86` | 改的是"改了脚本多久可见"的时序，语义敏感 |
| 12 | 工具侧 reqwest Client ~25 处逐次新建；最尖的在重定向循环体内（单图最多建 9 个） | `web_images/download.rs:493-511` 等 | 抽按 `(超时, 重定向策略)` 缓存；SSRF 钉 IP 的那处不能共享 |
| 13 | `conversation.db` 空闲页只在开库时回收一次；本机 31 MB 文件里 freelist 25 MB（80%） | `conversation_db/mod.rs:93-112` | **唯一带损坏风险的**（08-21 取证：incremental_vacuum 是失配放大器），只能低频、可关、最后做 |
| 14 | memory 库缺 cache_size；conversation.db 缺 mmap_size | `memory/mod.rs:374-395` | mmap 与低占用方向相反，量延迟 vs RSS |
| 15 | memory 联想拉 5000 行同步打分（P2 在册；`association()` 已改单连接贯穿，旧描述过时） | `memory/recall.rs:308-331` | FTS 会改排序，语义变化 |
| 16 | `MemoryStore` 三处每开连接重设 `journal_mode=WAL`（文件持久属性，建库一次够） | `memory/mod.rs:374-395` | 亚毫秒，顺手 |

**放大器**：daemon core 是 `current_thread` runtime + LocalSet（`web/actor/mod.rs:644-661`），多个回合跑在同一根线程。#3/#6/#7/#11 的任何同步阻塞代价是"所有会话一起停"，记账单位应是 毫秒 × 并发会话数。

### 3. 建议顺序

1. **第一批（几行改动、零语义）**：#1 #2 #5 #16 → 量冷启动 / RSS / 5 回合 wall time。
2. **第二批（补 08-18 漏网）**：#3 #4 → 造冻结探针先证明现象再修。
3. **第三批（要造 scaling probe）**：#6 #7 #8 → 旧实现当预言机做逐字节差分。
4. **量了再说**：#9 #12。
5. **最后且单独取证**：#13。

按 AGENTS 6.1，全部条目目前**没有实测数字**。形态判断有把握的是 #5 #6 #7 #13，量级判断没有。

**需要拍板**：是否按上述第一批开工；第一批做完带数据表回来再决定第二批。

---

## 调研来源

- AstrBot 原型：`/home/shorin/Documents/Astrbot/data/plugins/*/pages/dashboard/`，挂载机制在 `/home/shorin/Downloads/AstrBot/astrbot/dashboard/services/plugin_page_service.py`。
- Miyu WebUI 地形：`src/web/server.rs:263-383` 路由表、`web/app.js:9860-9890` 控制台面板、`web/shared.js` 独立面板范本、`styles.css:1-113` 令牌层。
- 已有裁定：`docs/plan/2026-09-03-optimization-survey.md`、`docs/plan/low-footprint.md`、`docs/fixed/2026-08-18-性能优化.md`。
