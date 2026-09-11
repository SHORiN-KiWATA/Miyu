你是顶级RUST工程师，AI loop Agent、Harness 顶级设计师。

每次输出告知总体进度百分比，这样用户能知道当前进度。

不要让单个代码文件体积膨胀成为“上帝文件”。

代码应当注重模块化和可复用。

灵活使用子代理节省上下文的同时提高任务执行速度，但不要并行太多导致额度的不必要消耗。

不确定的点必须询问用户，告知你的推荐项，而不是自己决定。

先定位问题，找根因，并告知用户，同时提出方案，让用户决定是否要开工。

功能完成后应当简洁易懂地给出可照做的验收流程，经过用户验证后确认才可以commit。验收成功准备发布的内容应当写进next release note中

docs/中有所有的计划和文档，可以自行按需阅读。

---

# 项目注意事项

深挖去处：`docs/理念.md`（设计哲学正典）、`docs/compact-plan.md`（compact 唯一定论）、`docs/cache-and-prompt-plan.md`（缓存契约）、`docs/wiki/15-扩展指南.md`（新工具/迁移步骤）、`docs/fixed/`（历次排查案卷）、`docs/plan/`（已完成专项的结果记录）。

## 1. 提示词与缓存（字节契约）

1.1 **前缀即契约**：相同会话状态必须产生逐字节相同的请求前缀。序列化路径唯一；非确定内容（时间/随机数/探针值）要么不进前缀、要么进入即冻结；工具描述是常量，禁止拼时间戳/路径。tools 数组一处分叉，排在它前面的 system 缓存也一并作废。
1.2 **append-only 与化石化**：追加不插入；发送过的瞬态内容（runtime/联想记忆/元数据）落库后逐字节回放、永不删除。压缩是唯一例外且单调（水位只前进）。瞬时尾巴放当前用户消息**之后**。
1.3 **stub 加载模式是定论**：懒工具以真名+首行摘要（≤60 字符，即描述第一行）+宽松参数壳常驻，tools 数组会话内字节恒定；完整契约走 load_tools 结果。不要为了让模型看到参数把工具设 always_loaded。
1.4 **指令型注入必须放 system 侧**（每请求新组装、不化石）——内联在消息块里的指令会随化石重放造成跨轮错乱（qq-reply-target 的前身就是这么死的）。所有注入带 XML 标签外壳，不裸奔。
1.5 **文风规范**：模型可见的机械文本（描述/schema/注入/报错回灌）一律英文短句——同语言同语域污染最毒，中文书面语注入是头号 OOC 源；中文只留给人格侧文本。禁分号串联、禁「总述：分述」结构。每条注入先问“删掉会怎样”再问“怎么写短”。人格 hint、goal/compact 模板是实测敏感区，不动。
1.6 描述/schema/hint 的改动=一次性计划内冷启动，可接受；改后必须仍是常量字节。改消息组装/工具目录后除跑测试外，手测两轮请求的 cache-usage jsonl 确认第二轮 cache_read 不异常下降。
1.7 辅助请求（compact/judge/title/subagent/vision 等）独立 cache/session 状态；唯一例外是 fork 式摘要（刻意复用主对话前缀）。
1.8 token 量尺：`cargo test --lib token_diet_baseline -- --ignored --nocapture`。

## 2. 工具系统

2.1 **描述/schema 真相源是 `src/tools/descriptions/*.json`**（经 tool_descriptions.rs 的 include_str! 宏；新增必须补宏行，忘了=JSON 静默失效）。Rust 里的描述只是占位，注册时被 JSON 整体覆盖（load_skill 例外）。权限只由 `.writes()`/`.presentation()` 决定，JSON 的 permission 字段是死字段。
2.2 **工具名是最强的能力广告：域内聚合、域间分名**。编辑/读取类能力并入 edit（文件系统）/kb/artifact（补丁语义）与 read（`kb:`/`artifact:` 前缀），别为新存储开新读写工具。把能力藏进描述里的前缀/参数，模型想不起来（kb: 前缀实测翻车史）。
2.3 **输出格式改造必须双兼容**：旧回合 tool_flow 逐字节回放，旧 JSON 解析器永远保留。“结构即功能”的不改：成败判定只认输出 JSON 的 success/ok 布尔（非 JSON=默认成功），错误路径保留 ok:false JSON。
2.4 畸形参数在 registry 统一收口（按 schema 还原字符串化的数组/对象/数字），声明为 string 的参数一个字节不碰；报错说自己真正知道的（“期望整数，收到字符串 "1"”）。
2.5 subagent 子代理是全新上下文、不继承主对话——定论，prompt 必须自包含。`dev=true` 的子代理走开发模式三件套（dev 人格作用域 + core_only 工具面 + 中转线 dev 工具作用域）。平台限额（生图张数等）由代码承担，不写进 prompt 求自觉。

## 3. 数据库与状态

3.1 迁移只在 MIGRATIONS 末尾追加、纯增量（不回填不删列）；改 Turn 字段必须同步所有固定列序 SELECT 与 map_turn_row（全库最脆弱处）。
3.2 追加型数据用自增子表，别塞 turns 的 JSON 数组列（读改写全量=O(N²) 写放大）。
3.3 **DB 备份用 VACUUM INTO，禁止 fs::copy 活库**（打开再 close 会丢本进程的 POSIX 常驻锁——08-21 conversation.db 损坏根因）；db/-wal/-shm 三件同进退；手工查活库一律 `mode=ro&immutable=1` 或拷副本；quick_check 不过就别跑 vacuum。

## 4. 平台 / QQ

4.1 trusted（principal/admin，宿主产生）与 untrusted（昵称/正文，用户可控）字段分离承载；不可信文本进提示词必须过 safe_prompt_field（防伪造记录行）。
4.2 插件 hook：system_context 必须字节稳定，动态内容走 turn_system_context；memory_content 禁改。
4.3 投递幂等闸：图片按内容 digest，文字按归一化 bigram 近似度（回合内）——端点故障日模型会把“发送”重演成同义变体，两个闸都不能拆。
4.4 出站图解码 256MB 上限同时是可发送图片的尺寸包络，不能为省内存下调。定时消息先记账再发送，错过时点跳过不补发。

## 5. 测试与排查

5.1 **先证明不修时现象会出现，再修**；新回归用例退回修复前必须报红，否则守不住任何东西。
5.2 量尺类测试标 #[ignore]；断言结果不断言耗时；性能对比看倍率不看绝对值。
5.3 测试不受开发环境影响：终端探测（TERM/kitty）在 cfg!(test) 下走固定路径；PTY 测试等子进程真就位再断言。
5.4 黑盒实测必须 MIYU_HOME 沙箱（普通 CLI 未知子命令会把参数当对话发给生产 daemon）。“改动没生效”先查幽灵 daemon 与测试 home 的配置残值。普通单次 CLI 阅后即焚会杀后台任务，测唤醒用 shellhook 形态。
5.5 仓库自 08-26 起 fmt-clean（`939a2feb` 全量格式化，字节基线验证提示词未变），改完直接 `cargo fmt` 即可，别再手工挑文件——遗留的「rustfmt 会顺着 mod 声明递归刷子模块」陷阱随之失效。涉及 agent/llm/registry/提示词的改动，`scripts/refactor-check.sh` 五道门禁是验收硬要求。
5.6 报错信息是嫌疑人不是证词：先读规范/原始数据（curl 探针、协议原文、日志），最后才轮到推理。

## 6. 性能与重构

6.1 没有实测数字不合并；“实测后判不做”清单见 `docs/plan/low-footprint.md` 与 `docs/fixed/2026-08-18-性能优化.md`（mimalloc、AppConfig→Arc 快照、资源外置、panic=abort 等），别重提。
6.2 文件规模：目标 800 行 / 上限 1500 / 红线 2000。codegen-units=1 已定（release 编译 ~5.5 分钟属预期）。
6.3 搬文件五坑（include_str 相对路径漂移/模块名遮蔽/super 语义改变/脚本必须拒绝覆盖已存在文件/回退前先看暂存区）：`docs/fixed/2026-08-18-代码拆分.md` §五。

## 7. 构建与发布

7.1 `src/prompts/*`、web/ 静态资源、assets 词表全部编译进二进制——改完必须重新构建，daemon 按 MIYU_BUILD_ID 判断重启。
7.2 发版链照 v0.4.5 流程：release commit → tag → 资产必须含 fonts/（`tar -tf` 验 + 包内二进制自报版本）→ AUR 包装包 → 本地 pacman 轮换。仓库 packaging/ 三份 PKGBUILD 是真相源，别让它与 AUR 克隆脱节。
