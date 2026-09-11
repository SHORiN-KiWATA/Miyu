# 2026-09-10 四项:历史占位符 / Safari 抖动 / tok·s / 事实整理

> 代码在 worktree `.claude/worktrees/main-fixes-deepseek`(分支 `worktree-four-items-2026-09-10-deepseek`,基于 `worktree-four-items-2026-09-10`,后者基于 main 1a8bf6e3),**未 commit**。
> 前一棒(fable)在 `.claude/worktrees/main-fixes-2026-09-09`(分支 `worktree-four-items-2026-09-10`),额度用尽时留下:项 3、1、4 已改,项 2 只定了改法。
> 用户裁定:一并做(占位符跨重启 + 图片);事实整理提示词是通用记忆系统,措辞不带「群友」;项 4 剩余三件全做;WebUI 刷新后历史回合**不保留** tok/s。
> 施工顺序 3 → 1 → 2 → 4。

## 状态(09-10 review 收尾:fable 复核 deepseek 全部改动)

| 项 | 状态 | 验证 |
|---|---|---|
| 3 tok/s | 施工完 | 定向单测 84 过;Chromium 真页面 meta 行 `每秒 486 tok · 本轮 2.5k · 累计 2.5k`;**REPL 真机** `78 tok/s`(testkit/repl-smoke) |
| 1 历史占位符 | 施工完 | 单测 `recalled_history_keeps_the_paste_placeholder_alive`、`history_file_round_trips_placeholder_payloads_and_reads_old_lines` 过;**真机已验**(testkit/repl-smoke:粘贴出 `[粘贴 1: ~4 行]`,发出后按上键回忆到的仍是占位符、不是四行裸文本) |
| 2 Safari 抖动 | **施工完** | 测具 A/B:WebKit `up_moves 0 / max_lag 1px / reversals 0`,Chromium `0 / 1 / 0`(修前 WebKit `1 / 3676px`) |
| 4 事实整理 | 施工完 | 记忆单测 45 过(含 6 条语义去重、4 条排序);沙箱 A/B 见 §4 |
| 5 shellhook 提问被取消 | **施工完(新加)** | 测具 `testkit/shellhook-question`:修前回合 `interrupted`、客户端 5.5s 自杀;修后 `completed`、答案落库 |

## 1. 上键历史带活占位符(已改,fable)

根因:历史存的是展开后的全文(`submit` 清空 `pasted_texts`,`record_history(&submission.content)`)。

改法:`cli/mod.rs` 新 `ReplHistoryEntry { display, pasted_texts: Vec<Option<String>>, images: Vec<Option<String>> }`;`LiveSubmission` 带 `pasted_texts`;`LiveReplOutcome::Submit` 第四元带 entry;编辑器 `history: Vec<ReplHistoryEntry>`,上下键 `recall_history_entry` 恢复载荷(旧路径 `read_repl_input` 用同一个 `restore_history_entry`);落盘:有载荷写结构体行、无载荷仍写裸字符串,读端两种都认;对话记录的展开全文与文件里的占位符条按 `expanded()` 认作同一条(`merge_history_entry`),带载荷者胜出。图片:`ClipboardImage` 记住 `cache_path`,`PastedImage::history_path()`,回忆时文件还在就接回 `PastedImage::Path`。

## 2. Safari 流式气泡抖动(已改,deepseek)

**根因**(fable 坐实):`.main-stage { container-type: inline-size }`(styles.css:910)。WebKit(WebKitGTK 2.52,与 Safari 同引擎)在容器查询容器的后代 DOM 被 `replaceChildren` 时把 `.chat-scroll` 的 scrollTop **同步归零**;随后 scroll 事件被当成用户上滚,`suspendOutputFollowing` 关掉跟随,视口停住/来回跳。zoom、fit-content、overflow-anchor、grid 换 flex、contain:size 都排除(页面有 CSP,注入 `<style>` 无效,实验走 CSSOM)。

**已改**:

1. `styles.css`:删 `.main-stage` 的 `container-type`;`@container (min-width:1360px)` 改成 `.main-stage.is-wide .stage-todos:not([hidden])`。
2. `app.js`:常量 `STAGE_WIDE_PX = 1360`;`syncArtifactLayout` 里 `classList.toggle("is-wide", mainStage.clientWidth >= STAGE_WIDE_PX)`(它在 `initialize()` 里已被调用,ResizeObserver 与 resize 也走它)。
3. 跟随逻辑加固:同一帧多次 `contentAdded` 合并成一个 rAF(`scrollFrame` / `scrollFrameSmooth`,不再用 `scrollRequestId` 互相作废——该字段已删);`programmaticScroll` 守卫改由 scroll 事件本身解除(smooth 保留 600ms 兜底);`linkcards.js` 卡片落地、`highlight.js` 补色后回调宿主 `contentAdded`(两者新增 `init({ contentAdded })`,app.js 注入),否则高度变化后视图停到下一段 delta 才跳。
4. `cargo build` 后测具 A/B(见下)。

**测具** `testkit/webui-jitter/`:`run.py`(沙箱 daemon + 桩长回复,`ENGINES=webkit,chromium`,`TAG=`,`JIT_PRELUDE=` 注入实验 JS)、`probe_webkit.py`(cage 无头 + PyGObject WebKitGTK;Playwright 的 webkit 缺 libicu74 跑不了)、`probe_chromium.py`、`sampler.js`(逐帧 scrollTop/scrollHeight + scroll/scrollTo/setTop/mutation 事件)、`metrics.py`(`valid` 要求容器真溢出)、`webkit_min.py`(最小页面对照,不复现)。

**A/B**(`TAG=after-09-10`,同机同桩):

| | up_moves | max_lag_px | reversals | final_lag |
|---|---|---|---|---|
| 修前 WebKit | 1 | 3676 | — | — |
| 修后 WebKit | 0 | 1 | 0 | 0 |
| 修后 Chromium | 0 | 1 | 0 | 1 |

## 3. tok/s(已改,fable)

`Usage` 加 `generation_tokens/generation_ms`(回合层测:每次请求首块到末块的墙钟,按回合累加,不含首字等待与工具执行;估算用量或单块请求不计);`GenerationSpeed` 视图;`UsageAccumulator::add_result` 返回本次请求 completion 增量(中转线结果帧是整轮累计,用差值);`RoundTiming` 记块时刻;`AgentEvent::RoundUsage` 加 `speed`;`chat.round_usage` 加 `turn_generation_tokens/ms`;`run.completed.usage` 自带。REPL:`TokenMeter` 加两字段,`format_tokens_per_second`(≥10 取整,<10 一位小数,测不到不显示),footer 四档级联最先丢速度;Token 行 `24.8k(C93%) · 361 tok/s · 26k/1M(2.6%) · Σ…`。WebUI:`formatGenerationSpeed` → `每秒 361 tok`,三处 meta 统一;**不落库**(用户裁定:刷新后历史回合无速度,省掉一次 turns 表迁移)。

## 4. 事实整理(已改,fable + deepseek)

真实库取证(123 条事实全为整理器产出):六成是通用技术问答全文;43 条超 120 字注入时被截断;前 5 条吃掉 30% 召回(华硕 s2idle 一条 238 次);字节级去重放过近义;时效新闻当永久事实;默认 Lite 档;坏项目整批失败无限重试(08-16 评审已记)。

**fable 已改**:`organizer.rs` 提示词重写(个人记忆系统不是百科;四类可存内容;判据「换个人问答案也一样就不是记忆」;单条 ≤120 字;近义算重复、矛盾用 update;会过期的写日期);`provider.rs` 组织器默认档 Lite→Standard;`write.rs` `apply_organized_batch` 单条校验失败只丢那条,批内日记一律清 `promotion_pending`;`validate.rs` 知识点上限 2000→300 字;`organizer.rs` 同批连续失败 4 次后 `skip_organization_batch`。

**deepseek 补齐(改法包第 2、5 条)**:

- **召回排序**(`recall.rs`):固定行形从 14 列扩到 17 列(加 `confidence`、`truth_status`、`recall_count`;episodes 无 truth_status 列,读常量),`map_hit_row` 返回 `HitRanking`。`score_bonus = strength×5 + importance + confidence×4 + truth_bonus − ln(1+recall_count)`;`accepted +3 / reported +1 / uncertain −2 / fictional −4`。对数饱和的疲劳惩罚把「靠 strength 自我强化」的霸屏事实按回同一起跑线。
- **语义去重**(新模块 `src/memory/dedup.rs`):
  - `widen_existing_candidates`:按批次日记文本嵌入检索,把语义相近的已有事实补进 `existing_memories`(上限 8 条,过与词面候选同一把可见性尺,复用库里的存量向量,现算向量上限 64)。模型看得见才有机会自己选 update。
  - `fold_near_duplicate_creates`:模型仍然发出的 create 里,归一化文本(小写/去空白标点/全半角)相同的直接改写;其余用嵌入找余弦 ≥0.88 的已有事实,改写成 `update` + `target_id`。**改写前用与落库同一套 validate 试跑**,不过就原样保留 create(多一条近义也比丢掉一条好)。
  - 阈值 0.88 是本机 bge-small-zh-int8 实测定的:无关内容 0.38、换个说法 0.88~0.93、「同主题结论相反」0.91 上下。后一档折叠成 update 正是提示词要的(矛盾用 update 改写而不是并存),旧内容留在 `memory_revisions` 里可查。
  - 现算的向量 `store_vectors` 写回 `memory_embeddings`,下一批整理直接复用;检索文本截到 1500 字(嵌入模型只认前几百 token)。
  - 接线:`organizer.rs` `process_job` 先 `widen_existing_candidates(&mut batch)`,再 `organize_batch(&store, job, &batch)`,后者在 parse 之后、apply 之前 `fold_near_duplicate_creates`。两条都是尽力而为,嵌入不可用只退化不报错。
- **测试**:`src/memory/tests/ranking.rs` 4 条(过度召回降权 / accepted 胜 uncertain / 把握高胜把握低 / 少量召回不沉底,退回旧评分公式 4 条全红)、`src/memory/tests/dedup.rs` 6 条(近义改 update、向量认出的换个说法、无关内容不动、可见性不匹配保留 create、没有嵌入器时扩展是空操作、语义扩展补候选且存向量)、`dedup.rs` 2 条单测。向量相关的 2 条要 ONNX Runtime,本机已装,跑得起来。
- **没做**(下一轮候选):提示词里让模型给「失效日期」的结构化字段;召回次数之外的「同一主题霸屏」检测。

**A/B**(`testkit/memory-quality/replay.py`,真实库拷进沙箱,最近 42 条日记重置为未整理,两边都固定走 ririxin deepseek-v4-flash,不烧 claude 额度;沙箱配置剥掉 platforms/voice/mcp/notifications):

| | 产出事实 | 平均长度 | >120 字 | 内容 |
|---|---|---|---|---|
| before(~/.local/bin 生产二进制) | 3 | 170 | 1 | Redox OS 百科、Arch npm 拆包通用知识、Gemini 测评观点;且 14 条日记卡住没整理 |
| after(fable 的二进制) | 3 | 145 | 2 | 本机 dsh 启动方式与路径(带日期)、本机没装 npm 用 bunx、dsh 标准模式工具配置路径(带日期);42 条全部整理完 |

**注**:本轮新增的语义去重/召回排序**没有重跑这个 A/B**——它会烧真实模型额度,且这两处要跑出差别需要「模型真的建了近义条目」的样本。建议验收时用 `testkit/memory-quality/replay.py` 再跑一趟 `TAG=after-dedup`(见收尾清单)。

## 5. shellhook 提问几秒后自己被取消(新加,deepseek)

**现象**:shellhook(fish hook 的 `printf '%s' "$buffer" | miyu --shell-intercept --shell fish --stdin`)里工具提问,面板开着什么都不按、或只按上下键,几秒后整个回合被取消。REPL 无此现象。

**根因**:`spawn_hangup_watchdog` 的 `terminal_hangup()` 裸 poll **stdin** 判挂断。管道写端(`printf`)一退出,stdin 常驻 `POLLHUP`;而 `question_tui::ask` 的 `QuestionSession::start` 正是看门狗的第一个调用点——面板一打开就按下「500ms 探测 + 5 秒宽限」的倒计时,到点 `std::process::exit(1)`。daemon 那边 `IpcRunGuard` 认这个客户端是一次性的(`origin_tty` 存在),断线即取消 run,回合以 `interrupted` 落库。REPL 两处都不满足:stdin 就是终端,连接也不带 `origin_tty`。

**改法**(`src/cli/mod.rs`):新增 `hangup_watch_fd(stdin_is_tty, controlling_tty)`——stdin 是终端才盯 stdin,否则盯控制终端(`controlling_tty_fd()`,`/dev/tty` 进程内只开一次),拿不到控制终端就永不判挂断;`fd_hung_up(fd)` 是原来的裸 poll。

**测具** `testkit/shellhook-question/`:`run.py` 用 `pty.openpty()` + `TIOCSCTTY` 复刻「stdin 是管道、stdout 是终端、有控制终端」这组 fd,桩模型立刻发 `ask_question`;`KEYS=arrows|answer` 控制按键。修前:面板 5.5s 后消失、客户端 `exit(1)`、daemon 日志 `one-shot client disconnected; its run was cancelled`、回合 `interrupted`。修后:面板一直在、上下键正常、回车提交后回合 `completed`、答案落进 `question_exchanges`。

**顺带补的真机测具** `testkit/repl-smoke/`(deepseek 加):PTY 里跑 `miyu normal`,粘 4 行 → 回车 → 上键,`report.json` 六项全绿 = 项 1 + 项 3 通过。两个坑记在它的 README 里:裸 `miyu` 在真终端先弹模式选择再退出(要用 `miyu normal`);回复刚打完立刻按上键会被重绘吞掉(先静置 1.5 秒)。

**单测**:`src/cli/tests/hangup.rs` 2 条(fd 选择逻辑;写端关闭的管道确实报 POLLHUP——记录这条真实形态,防止有人再把管道 EOF 当终端挂断)。

## 收尾清单

- 全量单测:`cargo test --lib` **2109 过 / 0 失败 / 32 ignored**(日志 `~/.cache/miyu-deepseek-fulltest.log`)。
- `cargo fmt` 已跑(仓库 fmt-clean);`node --check` 过了改动的三个 JS。
- 五道门禁 `bash test_scripts/refactor-check.sh`:格式 / 编译 / 全量测试(**用例数 2112,基线 2088,0 失败**)/ 模型面语言(`no CJK, all JSON valid`)/ 依赖方向 全过;**文件规模那道报红,但它是存量问题**——基线停在拆分刚合入 main 时的 198,995 行,现在任何提交都比它多 30%(`cd ~/Documents/github/Miyu && python3 test_scripts/refactor_size_report.py --check` 在干净 main 上同样报 `+29.8%`)。本轮的增量是 258,338 → 259,651 行(+0.5%),没有新增越红线文件、超标文件也没变长。要修得单独重写基线。
- **未 commit、未写 release note**(等验收)。验收后按 AGENTS.md:commit → `next-release-note.md` 追加(修复:上键历史占位符、Safari 抖动、shellhook 提问被取消;重要更新:tok/s、事实整理)→ merge。

## release note 草稿(验收通过后再挪进 `next-release-note.md`)

修复:
- 在 shell 提示符里直接跟她说话(shellhook)、她反问你一个问题时,面板不再几秒后自己消失、整轮被取消。根因是挂断看门狗把「stdin 是管道、写端已退出」当成终端挂断(那一轮命令正是用管道喂进来的),5 秒后自杀,服务端看到一次性客户端断线就把回合掐了;现在改盯控制终端。
- WebUI 在 Safari 里流式输出时气泡尾部上下抖动、视口被拽回顶部。根因是一处容器查询:WebKit 在容器查询容器的后代被整块替换时会把聊天区滚动位置归零,而 Chrome/Firefox 有滚动锚定兜底。现在宽度判断改由 JS 挂类名,并顺手加固了自动跟随(同帧滚动合并、程序滚动守卫由滚动事件解除、链接卡片和高亮补色后通知滚动)。
- 终端里按上键回忆历史时,粘贴的多行文本不再变回几十行原文(历史现在记住占位符和它对应的载荷,重启后仍然有效,图片附件一并保留)。

重要更新:
- 终端 footer 与 WebUI 的消息元信息新增**每秒输出速度**(按每次模型请求「首个流式块到最后一个块」计时,不含首字等待与工具执行;估算了用量或测不出时不给数字)。
- 事实整理器重做:只记人、本机/本项目、带日期的实测结论与约定,通用知识和技术教程不再入库;单条限 120 字;默认档位从最便宜的池提到标准池;单条坏数据只丢那一条、连续失败会放弃整批而不是每 5 分钟重试;召回排序纳入可信度与把握,被反复召回的条目按对数降权;建条前用向量做语义去重,换个说法的近义事实改成改写已有条目而不是再存一条。

## 验收流程

前置:`cd .claude/worktrees/main-fixes-deepseek && cargo build`(web/ 与提示词都编进二进制,daemon 按 MIYU_BUILD_ID 重启)。

1. **项 1 + 项 3(一条命令)**:`python3 testkit/repl-smoke/run.py`,看 `report.json` 六项全绿(`placeholder_on_paste` / `reply_seen` / `footer_speed` / `placeholder_on_recall` / `raw_text_on_recall=false` / `repl_alive`)。手动版:REPL 里粘贴一段多行文本(出现 `[粘贴 1: ~N 行]` 占位符)→ 回车发出 → 按 ↑ 回忆 → 输入框应显示占位符而不是全文;再回车应原样发出全文;重启 REPL 后按 ↑ 仍应带占位符。
2. **项 2 Safari 抖动**:`python3 testkit/webui-jitter/run.py`(要 cage + WebKitGTK + playwright chromium),看 webkit 那行 `up_moves 0 / max_lag_px` 是否与 chromium 同量级;真机 Safari 里让一段长回复流式输出,视口应平滑跟到底、不上下鬼畜。
3. **项 3 tok/s**:REPL 发一句,footer 应出现 `NNN tok/s`;WebUI 发一句,meta 行应出现 `每秒 NNN tok`;刷新页面后历史回合不显示速度(预期)。窗口拉窄,footer 应最先丢速度、再丢 Σ、最后丢百分比。
4. **项 4 事实整理**:`python3 testkit/memory-quality/replay.py`(会烧真实额度)对比 `TAG=before` / `TAG=after-dedup` 的条数与内容;或在沙箱里让她记两条近义事实,看库里应只有一条、且有 `memory_revisions` 记录。
5. **项 5 shellhook 提问**:在 fish 提示符里直接说一句话触发提问(或 `KEYS=answer python3 testkit/shellhook-question/run.py`),面板应一直等到你回答;什么都不按放一分钟也不该消失;上下键应正常移动选中项。
6. **回归**:`bash test_scripts/refactor-check.sh` 全绿。

## Review 收尾(09-10,fable)

- deepseek 改动逐文件复核:第二项与交接单改法一致;第四项补齐语义去重(`dedup.rs`)与召回排序;新增第五项 shellhook 提问被取消(看门狗改盯控制终端)。全量单测 2109 过、fmt 干净;refactor-check 唯一红的「总行数增长」门在 main 上本就红。
- 改了一处:`scrollToBottom` 非 smooth 路径补 150ms 兜底超时(视口已在底时 `scrollTo` 不派发 scroll 事件,守卫会挂住、吞掉用户下一条滚动事件)。重建后抖动 A/B 不变:WebKit up_moves 0 / max_lag 1px,Chromium 0 / 1px。
- 记忆 A/B 补跑 `TAG=after-dedup`(deepseek 二进制,同模型 deepseek-v4-flash):42 条日记 → 2 条事实,平均 97 字、0 条超 120 字,memory_revisions 34→38(有近义被折成 update);内容是 uinput 操控约定与本机拼接桌面分辨率,都是「这台机器、这个人」的事。
- 提醒(未改):去重阈值 0.88 会把「同主题结论相反」折成 update,部署后看 `memory_revisions`;stdin 是管道且无控制终端的进程不再判挂断。
- release note 已写进主检出 `next-release-note.md`(修复 3 条:上键占位符、Safari 抖动、shellhook 提问;重要更新 2 条:tok/s、记忆整理)。
