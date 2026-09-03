# 2026-09-03 全项目优化调研（未开工，记录备查）

结论：用户裁定**当前不改**（"用着没什么问题"）。本文只记录调研过程、已核实的证据与
方案复核结果，供以后重提时直接接着用，避免重复调研。

调研方式：五路子代理分别扫「Rust 运行时性能」「WebUI」「终端 REPL」「安全与工程面」
「产品功能面」，每路先读 `docs/plan/low-footprint.md`、`docs/fixed/2026-08-18-性能优化.md`、
`docs/plan/token-diet/` 的"判不做"清单排除旧项；所有硬断言由主会话用 grep / 命令复核。
原则同 08-18 专项：**没有测量数字不合并**，本文所有性能项均未实测，都是嫌疑人。

## 0. 本次实测的硬指标（2026-09-03，main @ 9b98d58c，v0.4.6）

| 指标 | 数值 |
|---|---|
| Rust 源码 | 539 文件 / 208,263 行 |
| `cargo test --offline` | 1697 lib + 3 集成用例全过，28 ignored，总耗时 13 s，无网络 |
| `cargo clippy --lib --bins` | 137 warning / 0 error（`--all-targets` 186） |
| `cargo fmt --check` | 9 处 diff（AGENTS 5.5 "自 08-26 fmt-clean" 已漂移） |
| 非测试代码 `.unwrap()` | 490 处，其中 397 处是 `lock().unwrap()` |
| panic hook | 无（`set_hook` 零命中） |
| daemon RSS（空闲） | 58 MB |
| release 二进制 | 51.0 MB（.text 33.2 MB / .rodata 11.1 MB） |
| 嵌入资源 | jieba FST 3.2 MB、o200k 词表 1.8 MB、web 静态 ~1.2 MB、KaTeX 字体 0.6 MB；CJK 字体 30 MB **未嵌入**，外部路径查找 |
| WebUI | app.js 10,779 行单 IIFE，styles.css 7,551 行，无构建步骤 |

## 1. 确定是 bug（已逐条核实）

| # | 问题 | 位置 | 后果 | 修法是否零语义 |
|---|---|---|---|---|
| B1 | MSRV 声明 1.89，两处用 1.95 才稳定的 `AtomicUsize::try_update` | `src/platforms/scheduling.rs:86`、`src/tools/vision/reference.rs:163` | 按声明版本构建失败 | 是，`fetch_update` 同签名同语义 |
| B2 | dev 模式 REPL 会话指针解析失败时静默新建空会话（根因已在注释里写明，分支未改） | `src/web/ipc_server.rs:200-207` | 无历史 / 上键无记录 / `/undo` 失效（08-26 报的三连） | 否，需选"报错"或"回落最近可用会话" |
| B3 | `origin_is_allowed` 硬编码 `http://{host}` | `src/web/security.rs:327` | 挂 HTTPS 反代后所有写端点 403 | 放宽为同时认 https |
| B4 | 闹钟 worker 进程死亡后记录被 `cleanup_dead` 静默清除 | `src/alarm.rs:136-144`、`src/cli/alarm_worker.rs:36` | 重启后闹钟不响也不报错 | 否，需改成标记失效 |
| B5 | REPL 宽度函数把组合记号算 1 列、ZWJ emoji 逐码点算 2 列；文件头注释承诺零列但未实现 | `src/cli/repl/width.rs:37-56` | 中文/emoji 光标错位 | 基本是；边缘变化：PUA nerd-font 图标 2→1、U+FE0F 1→0；`wrap_visible_width` 需改逐 grapheme |
| B6 | 代码块框线宽度用 `chars().count()` | `src/render/code.rs:118,150` | 含中文的代码块框线错位 | 是 |
| B7 | 代码高亮对所有语言无条件认 `//` 为注释 | `src/render/code.rs:29` | Python `a // b` 整行注释色 | 是，用排除表（py/sh 族/toml/yaml）而非白名单 |
| B8 | config_tui 宽度 `.min(cols-4).max(56)` 顺序颠倒 | `src/config_tui/widgets/draw.rs:24,89` | 窄终端框溢出 | 是 |
| B9 | config_tui 只收 `Event::Key`，丢 Resize | `src/config_tui/widgets/draw.rs:349-362` | 改窗口后画面错乱 | 是 |
| B10 | 图片路径用 `is_native_kitty_terminal`，公式路径用 `kitty_graphics_supported`（含 ghostty） | `src/tools/vision/print.rs:76` vs `src/render/math/mod.rs:505` | ghostty 下公式走原生协议、图片走 chafa | 是，两路共用同一 `terminal::kitty` 序列生成 |
| B11 | QQ 入站路径两处 `expect` 靠上游分支保证不变式，输入用户可控 | `src/platforms/onebot/dispatch.rs:753-754` | 分支改坏即 daemon panic | 是 |
| B12 | RSS 解析 `current.as_mut().unwrap()`，畸形 feed 可触发 | `src/tools/archlinux.rs:688` | 网络输入导致 panic | 是 |
| B13 | 无 panic hook；397 处 `lock().unwrap()`，任一线程 panic 毒化 DB 锁后连锁崩 | `src/state/conversation_db/*` | daemon 整体倒下且无 backtrace | 是（只加日志钩子） |
| B14 | `rustyline` 零引用仍在依赖表 | `Cargo.toml:52` | 白付编译时间与体积 | 是 |
| B15 | 首次运行 5 步各 sleep 180ms 加一次 sleep 1s | `src/cli/setup.rs:80,91` | 近 2 s 人造延迟 | 是 |

四套显示宽度实现并存：`render/table.rs:234`、`cli/repl/width.rs:37`、`render/command.rs:614`
（唯一正确，unicode-width + grapheme）、`config_tui/widgets/draw.rs:391`。B5/B6 的根修是收敛到第三套。

## 2. 性能嫌疑（未实测，均需先造 scaling probe）

| # | 现象 | 位置 | 频率 | 方案复核结论 |
|---|---|---|---|---|
| P1 | `effective_context_tokens` 每次全量读库、重建消息数组、重估 token；REPL 侧 21 处调用 | `src/agent/setup.rs:379`、`src/agent/history.rs:111` | 每回合 5–8 次 | memo 键不能用 seq（turns 会原地改写；`pruning.rs:216` 折叠后立刻读它决定是否跳过压缩，漏失效=错跳压缩）。应以 `total_changes()` 做写世代：非测试代码 conversation.db 只有一处 `Connection::open`，所有写过同一把锁。但流式期间 journal 每 80ms 写一次，回合中途几乎不命中，收益集中在回合间隙。另一条路是 `prepare_cached` + 消掉防失忆提醒路径（`context.rs:845`）的重复加载 |
| P2 | 记忆联想 facts/episodes 各拉 5000 行同步打分，跑在 tokio worker 上无 `spawn_blocking`，随记忆库线性增长 | `src/memory/recall.rs:295-399`，调用 `src/agent/turn_loop/stream.rs:100` | 每回合 | 与 08-18 修好的 kb `keyword_search` 同型漏网。`spawn_blocking` 是**公平性修复不是延迟修复**：本回合照样等，受益的是 daemon 其他会话。降延迟得把过滤推进 SQL/FTS，会改排序结果 |
| P3 | 每个 LLM round 重建整份工具目录（61 条深拷贝 + stub 摘要扫描 + 排序），而数组会话内字节恒定 | `src/tools/registry/spec.rs:304`、`lazy.rs:80`、`turn_loop/mod.rs:112` | 每 round | 注册表变更点只有 register / unregister / set_default_timeout 三处，加世代计数缓存成 `Arc<Vec<ToolDefinition>>` 安全。顺带消掉 C13 留下的"命中路径也要先序列化"尾巴（`context.rs:218-231`，实测固定 1.63 ms×3/回合） |
| P4 | 消息数组每 round clone 4 次，其中一次在端点重试循环内 | `src/agent/turn_loop/mod.rs:135,167`、`src/llm/openai_compatible/chat.rs:267,413` | 每 round×attempt | 改签名收 `&[ChatMessage]` 或 `Arc<[_]>`。keepalive 快照那处（`turn_loop/mod.rs:161`）08-18 已判不做，不在此列 |
| P5 | 每条 journal 事件一个独立 `BEGIN IMMEDIATE` 事务 + 一条三表 JOIN 校验 | `src/state/conversation_db/turns.rs:79-115` | 流式每 80ms | **"缓存校验结果"的想法是错的**：那条校验是防 supersede 后旧写手继续追加的守卫。零语义改法只有合成一条 `INSERT…SELECT…WHERE EXISTS`，省往返不省事务，收益小；批量提交改可见时序 |
| P6 | MemoryStore 每次查询新开连接并重设 `PRAGMA journal_mode=WAL` | `src/memory/mod.rs:374-396`，22 处调用 | 每回合 2–4 次 | MemoryStore 是 Clone 且多任务共用，共享单连接会让 organizer 与当前回合互相排队，可能更慢。只删 journal_mode 那句（WAL 是文件持久属性），预期亚毫秒 |
| P7 | 全库 40 处 `conn.prepare`，0 处 `prepare_cached`；attach_* 动态拼 900 占位符 | `src/state/conversation_db/rows.rs:568` | 每次会话加载 | 静态 SQL 全换 `prepare_cached`；索引侧已核无缺失 |
| P8 | 人格提示词每回合读盘两次（fingerprint 再读一次）+ `init_files` 四次 syscall | `src/agent/setup.rs:213-243`、`persona_paths.rs:205-242` | 每回合 | 按 mtime 缓存 |
| P9 | `recover_journal_assets` 为找一个 turn `load_turns` 全量；`recover_stale_turns` 循环内调用 → O(N²) | `src/state/assets.rs:247-256`，`turns.rs:247` | 每次 interrupt / 启动 | turn_id 是主键，加单行查询即可 |
| P10 | 出站图 20 MiB blake3 + base64 在 tokio worker 上同步；去重闸 `std::fs::read` 同步读整图 | `src/platforms/onebot/send.rs:88-197`、`outbound.rs:131`、`platforms/tool.rs:247` | 每张出站图 | 并进已有的 `validate_outbound_image` spawn_blocking；不动任何限额（low-footprint 1.6 判不做的是下调上限） |
| W1 | WebUI 流式渲染每帧对累积全文重跑 markdown + KaTeX，整块 DOM 替换，O(n²) | `web/app.js:6830-6838`、`:4903` | 每帧 | 增量渲染要处理围栏/跨空行列表/表格/数学块边界，风险高于预期。零风险先做：KaTeX 结果缓存 |
| W2 | 回合运行期间每秒两次对整段历史 `JSON.stringify` 脏检查，不同就 `replaceChildren` 全量重建 | `web/app.js:8716-8724`、`:6357` | 每秒 | 改比 `(turns.length, 末回合 seq/updated_at)`；时间线加 `content-visibility: auto` |
| W3 | 90ms `querySelectorAll` 盲文 ticker 永不清除；设置面板每次按键序列化整份 config | `web/app.js:3369`、`:1120` | 常驻 | 换 CSS steps() 动画；debounce |

已核对无问题、可从待办摘掉：`std::sync::Mutex` 跨 await（`turn_loop/mod.rs:83,602`、`control.rs:240` 都在 await 前结束）；
`src/tools/mod.rs`（非测试 730 行）与 `apply_patch.rs`（非测试 936 行）不超 AGENTS 6.2 目标，不需拆；
真正逼近上限的是 `src/agent/turn_loop/mod.rs` 1158 行零测试模块。

## 3. 安全面（用户裁定搁置，记录以备重提）

| # | 问题 | 位置 | 复核 |
|---|---|---|---|
| S1 | OneBot 反向 WS 无 Origin 校验；`access_token` 空时只看 peer loopback，浏览器网页的 WebSocket peer 就是 127.0.0.1，可伪造 admin 的 QQ 事件拿宿主工具面。配了 token 即免疫 | `src/platforms/onebot/connection.rs:355-406` | 已核，无 Origin 读取。修法约 20 行：存在 Origin 头即拒 |
| S2 | WebUI 默认绑 0.0.0.0 且密码可选，无密码时 `is_authenticated` 恒真 | `src/web/server.rs:45`、`src/runtime/state.rs:228` | 已核。推荐：保留默认但非回环且无密码时拒绝启动 |
| S3 | `web_fetch` 无 SSRF 防护（无内网 IP 判定、无 no_proxy、重定向不逐跳校验），且进受限注册表给群友 | `src/tools/web/mod.rs:100,275-305`、`src/tools/mod.rs:648` | `web_images/download.rs:645-666` 已有三层防护可提成公共 helper |
| S4 | `qq_withdraw_message` 无任何请求者身份门 | `src/platforms/plugins/message_recall.rs:320-340` | 已核。`qq_group_manage` 的"AI 自审"是有意设计，不推翻；建议只把 kick/blacklist 升硬门 |
| S5 | `ToolPermission.writes()/presentation()` 运行时无人读（5 处 `.permission()` 全在测试），真实边界是 `restricted_platform_registry` + 插件 `is_admin`；AGENTS §2.1 说法失真 | `src/tools/registry/mod.rs:267` | 建议加受限注册表白名单快照测试 |
| S6 | `command_deny` 是子串匹配，`rm  -rf /` 即绕过；文档把它列为护栏 | `src/tools/mod.rs:365-378` | 建议文档降级为"防误触提示名单" |
| S7 | 后台 job 无超时无输出上限，与文档"120s / 8MiB"不符 | `src/tools/jobs/mod.rs:302-345` | — |
| S8 | 入群审批提示词中文且 `comment` 未过 `safe_prompt_field` | `src/platforms/onebot/group_join.rs:214-229` | 其余 61 处不可信文本入口均已覆盖 |
| S9 | `request_log` jsonl 与 `daemon.log` 未显式 0600；存量 `config.jsonc.bak-*` 为 0644 含明文 key | `src/llm/request_log.rs:83`、`src/ipc/lifecycle.rs:486` | — |

脚本执行面复核良好：argv 直执行不经 shell、stdin JSON 传参、120s 超时封顶 300、8MB 输出上限、路径 canonicalize 沙箱、不进受限注册表。

## 4. 工程面漂移

- 无 CI。`cargo test --offline` 13 s 全过，网络测试均已标 ignore，`tests/daemon_reload.rs` 用 tempfile + MIYU_HOME 隔离可进 CI。加 fmt+clippy(不带 -D warnings)+test 三道门约 20 分钟，前提先清那 9 处 fmt diff。
- `scripts/refactor-check.sh` 实际在 `test_scripts/refactor-check.sh`，AGENTS 5.5 与 wiki/14 §4 路径写错。
- 三份 PKGBUILD 停 0.4.5，Cargo.toml 已 0.4.6（发版中间态，但无机制提醒）。
- `docs/wiki/05` 的 `loading_mode` 默认值仍写 stub 且介绍已删除的 hybrid 档；代码 09-01 已改 full。
- `docs/wiki/08` §3 读起来像"配了 embedding 联想就会用"，实际 `recall.rs` 无 embedding 分支，只有 `evicted.rs:214` 一处用向量。
- 模型可见的中文错误回灌 5 处（`message_history/tools/mod.rs:587`、`message_recall.rs:155,184,197`、`access_manager.rs:98`），违反 AGENTS 1.5。
- `test_scripts/check-model-english.sh` 已存在，可挂 CI。
- crossterm 0.28（直接）与 0.29（经 termimad→coolor/crokey）双版本；fancy-regex 0.14（ratex-parser）与 0.17 双版本。termimad 唯一用途是 `miyu history` 回放，导致 history 与 REPL 观感是两套。
- 六种哈希 crate 各司其职（sha1/sha3/md5/blake2/crc32fast 五个全部且仅服务 `hash_codec` 工具），不建议收敛。
- `[profile.release]` 未设 `opt-level`（默认 3），`lto = "thin"`；可量一次 `lto = "fat"`。

## 5. 功能与体验缺口（按价值排序，用户裁定搁置）

1. **定时任务**：闹钟到点只是 `thread::sleep` 后播音（`src/cli/alarm_worker.rs:36`），不能触发回合、不重复、重启即丢。daemon + goal 续轮 + job_wake 回写三套基建齐全，缺调度器。README 154-158 的宣传与此落差最大。
2. **辅助任务全走主池**：compact（`agent/compact.rs:82`）、organizer（`memory/organizer.rs:246`）、会话标题（`web/sessions.rs:541,713`）都 `from_config` 主池；`SubagentTiersConfig` 三档池已存在，改 4 个调用点即可。
3. **记忆无浏览/单条删改**：CLI 只有 stats/search/remember/reset，WebUI 只有 reset；数据层含 `memory_revisions`，纯 UI 工作。去重是逐字节精确匹配（`write.rs:406-413`）。
4. **MCP**：每次调用新起子进程（`src/tools/mcp.rs:164-172`），有状态 server 用不了；无 HTTP/SSE；config TUI 无 MCP 项。
5. **dev 模式**：无 git 只读工具、不读项目 AGENTS.md/CLAUDE.md、无文件级 undo、B2 未修。
6. **voice 分支**：落后 main 264 commit，改的 `src/web.rs`、`src/tools/memes.rs` 在 main 已不存在；建议先只合 TTS 约 400 行。
7. 缺 `miyu usage` / `miyu doctor` / `miyu history --export`；无版本更新提示（默认知识库反而有）。
8. 平台配置 `platforms.qq` 硬编码单字段（`config/platform.rs:94`），web 侧 12 文件有 qq 硬编码；上 Telegram 前必须先泛化。
9. 终端：颜色 100% 硬编码，`PRIMARY_STYLE` 256 色 189 在亮色终端不可读；无 `NO_COLOR`、非 TTY 不自动 plain（全仓仅 `daemon_log.rs:24` 读 NO_COLOR）；config_tui 零颜色；缺 Ctrl+A/E/K/U、Ctrl+R；Tab 只在唯一候选时补全；`![](url)` 不出图；chafa 缺失时图片直接报错而公式有三级降级；费用显示 `usage_view.rs` 文件头承诺了但未实现；`is_native_kitty` 精确等于 `xterm-kitty`。
10. WebUI：无跟随系统主题（`setTheme` 里 `[data-theme-choice]` 是死代码）；无 gzip（首屏 ~700 KB 明文）；设置面板无 `platforms.*` 表单；`/workspace` `/history` 无 GUI 入口；`.session-time` 9.5px 对比度约 3.4:1；模态无 focus trap。做得好的：移动端适配、reduced-motion、隐藏页暂停动画、CSP 无 unsafe-inline、SSE 双水位重放。
11. 已知搁置项的两条新根因线索：回车光标瞬移——`tail/frame.rs:53-56` 与 `layout.rs:71-74` 两处显式 `MoveTo(0, rows-1)` 都在同步块外，pyte 重放测不到；kitty 滚动残影——`lift_external_output_into_page` 未显式清 DECSTBM，依赖上一帧路径。

## 6. 复核中被推翻的自家方案（避免下次再犯）

- P5 "缓存 EXISTS 校验"：拆守卫，错。
- P1 "按 seq 做 memo 键"：turns 原地改写，漏失效会错跳压缩，错。
- P2 "spawn_blocking 降延迟"：只改公平性，不降本回合延迟。
- P6 "复用连接"：多任务共享单连接可能更慢。
- B5 "ASCII 和中文结果不变"：PUA / VS16 边缘会变。
- W1 "只重渲最后一个未闭合块"：块边界判定风险高，先做缓存类零风险项。
