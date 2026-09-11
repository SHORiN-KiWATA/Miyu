# Miyu 架构（as-built，2026-09-11）

配套架构图：https://claude.ai/code/artifact/20ce2a89-cd70-41f4-a25a-36bdb303f2ea

本文写的是**已经落地**的架构（分层重构七阶段全部合入 main，v0.5.x 线）。设计过程与
取舍记录在 [`plan/2026-09-10-layered-architecture.md`](plan/2026-09-10-layered-architecture.md)；
凡与那份计划不一致的地方，本文标了「与计划的出入」，以本文为准。

---

## 一、三层代码

从下往上，下层不依赖上层，上层经挂接接口注册进流水线。

### core —— 自己就能当 agent 用
不依赖上面任何一层，等于今天的 dev。挂接接口在这里定义。

- **回合引擎** `src/agent/`：`turn_loop` 模型↔工具循环、`prompt` 提示词骨架、
  `history` / `context` 请求组装、`compact` / pruning 压缩溢出、`control` 单跑闩锁与续传、
  `repeat_gate` 复读闸。
- **核心工具**（约 11 件）：`run_command` + `jobs`、`apply_patch`、`todowrite`、`goal`、
  `subagent`（子代理，`dev=true` 走开发模式）、`web` 搜/取、`vision`、MCP 客户端、
  `load_tools`、`ask_question`。
  `ask_question` 只在「能弹问题」的入口给。
- **模型客户端与池** `src/llm/`、`src/config/`：OpenAI 兼容 / Anthropic / 中转线
  （claude-code / codex / antigravity）；全局池 + 四档 Lite / Cheap / Standard / Flagship。
- **状态与 daemon** `src/state/`、`src/web/actor`：会话 / 回合 / 历史 / 队列、用量与计费、
  附件；单 actor 串行跑回合；IPC socket + HTTP/SSE。

### 扩展层 —— 今天叫 normal 的那部分
依赖 core，经挂接接口注册。按**挂接点**分两种，不按来源分：

- **子系统**（挂进流水线多个点，编译进来）：记忆（工具 + 每轮联想注入 + 回合后写日记/经历
  + 逐出库归档 + 系统提示前言）、人格提醒（化石注入，按间隔）、情绪/好感度（提示词 +
  QQ 面板 + dashboard）、语音（唤醒 · 听写 · TTS · 协议片段）、技能扫描（目录 →
  `load_skill` / `manage_skill`）。和压缩、提示词组装咬在一起，市场装不了。
- **插件**（只往工具面加东西，persona 看不出内置与外装的区别）：
  - 内置（编译进）：ledger、knowledge_base、memes、alarm、image_generation、web_images、
    deep_research、package_advisor、diagnostics、usage_query、api_quota、send_qq_message。
  - 外装（目录扫描）：scripts、skills、MCP 服务器、插件包。
  - 每件清单声明五个字段：trust 位、分组归属、指路句、跨工具闸、附件投递
    （阶段 1 已补，`src/tools/scripts/header.rs`）。

### 场所层 —— 入口只声明两件事
不拥有工具，只附胶水、按信任过滤。见「四」。

---

## 二、一回合的流水线

从入口收到一句话到回复渲染出来，十步。子系统只在三个挂接点（A 联想注入、B 逐出库归档、
C 回合后钩子）和系统提示前言处进入；插件只在「组工具面」那步加工具。

1. **入口收到一句话**（场所层）：谁说的、有无附件、来自哪个入口。
2. **找到会话**：读出冻结的 persona 指针和场所属性；套 `TurnOverrides`。
3. **组系统提示词**：骨架 ← core；人格全文 + 属主档案 ← persona；记忆前言、语音协议 ←
   子系统；host-environment 与 LaTeX 一句 ← 场所能力位。
4. **组历史 + 工具面**：核心 11 件 ← core；启用的扩展 ← persona；胶水 + External 过滤 ←
   场所；full / stub 档位 ← 模型能力。
5. **记忆联想注入**（子系统挂接点 A，`turn_loop/stream.rs`）。
6. **调模型**（core）：池 = 回合覆盖 > 平台引用 > 全局池；前缀逐字节稳定吃缓存。
   **发请求前配平** tool_calls / tool 结果（`context::enforce_tool_call_result_balance`），
   任一路径漏了一条 tool 结果就补占位，防严格网关 400。
7. **工具分发与闸**（core，闸由扩展清单声明）：命令拒绝串、AUR 先审后装、复读闸、
   并行有序执行；**成员回合里工具套 Landlock 沙盒**。
8. **回到 6 直到没有工具调用**：中途溢出先落盘工具输出再压缩；逐出的回合归档
   （子系统挂接点 B，`context.rs`）。
9. **回合后钩子**（子系统挂接点 C，`stream.rs` 的 `process_after_turn`）：写事实/经历/日记、
   情绪更新、人格提醒计数、落库生成速度。
10. **回到入口渲染**（场所层）：终端画 diff / kitty 图；WebUI 开 artifact；QQ 转图/语音。

dev persona 启用集为空：第 3 步只有骨架和一行提示词，第 4 步只有核心 11 件，A/B/C 不构造。
这就是「只构造启用的」，不是「装了再关」。

---

## 三、persona = preset

配置只有一种。一个目录 = 一个 persona = 提示词 + 关系档案 + 启用哪些扩展。`mode` 概念退场，
`miyu dev` 只是切到 dev persona。

- **共享 persona**：`personas/<name>/`，管理员发布，成员只读。当前有 `default`（出厂 Miyu，
  全扩展开）和 `dev`（启用集为空）。记忆一个库、三层可见性（privileged 只管理员 / principal
  只本人 / public 所有人）。
- **私有 persona**：`home/<user>/personas/<name>/`，用户 OOBE 自建。自己一个记忆库、不分层；
  提示词本人可改；启用集 ⊆ 管理员白名单；缺的扩展装前提示。角色扮演的主场。

> **与计划的出入**：计划要把出厂人格目录从 `default` 改名 `miyu`；as-built 仍叫 `default`。

---

## 四、场所只声明两个属性

| 入口 | 信任 | 能力 | 由此推导 |
|---|---|---|---|
| 终端 REPL | Owner | 可弹问题 · 终端渲染 · kitty 图 | `ask_question`；diff/图在终端画 |
| stdio / ask / shellhook | Owner | 纯文本 | 不给 `ask_question`；程序驱动用 `TurnOverrides` |
| WebUI | 管理员 Owner / 成员 Member | 可弹问题 · 浏览器 · LaTeX | artifact · share；**成员回合套沙盒** |
| QQ 私聊 / 群 | External | 图 · 语音 · 长文转图 | 按 trust 位过滤；每条带发送者；平台池引用 |
| 语音唤醒 / 定时 / 闹钟 | Owner | 无面板 · 可播报 | 不给 `ask_question`；回复走 TTS 或通知 |
| 子代理 | Internal | 无面板 | 同 persona；工具面是父回合快照；池按 tier |

**信任解析顺带产出 principal**：入口、账号、用户 id 三元组哈希得到的稳定键，随会话冻结；
记忆隔离、用量归属、沙盒根都从它派生。跨端进同一会话不重算工具面，用不了的工具报
「此入口不可用」。

### 系统提示词（as-built）
- style-lock、语音协议与受众无关，回到人格路径。
- 属主档案（`home/<user>/profile.md`）只在属主类入口注入，通讯平台不生效。
- **host-environment 保留**，WebUI 回合（External 非平台）也带，成员沙盒回合里能看到自己的
  工作区；QQ 等平台回合不带。**其中不含 effort**——档位对话中会切，写进提示词会掰断前缀缓存。
- LaTeX 一句由场所能力位决定。

> **与计划的出入**：计划要「host-environment 删」；as-built 反过来**保留并扩展到 WebUI**，
> 只从中去掉了会变的 effort。原因：成员沙盒回合需要知道自己的工作区在哪。

---

## 五、目录（as-built，仿 Linux）

根目录是系统，`home/<user>/` 是人。**迁移是部分的**：属主个人数据进了 `home/shorin/`，
但机器级与共享数据仍留在根 `data/`（计划设想的一次性全量迁移未做完）。

```
~/.miyu/
├── config/              机器级配置（config.jsonc、shell/、scripts/、skills/），≈ /etc
├── personas/            共享人格，管理员发布、成员只读，≈ /usr/share
│   ├── default/         出厂 Miyu，全扩展开
│   └── dev/             启用集为空
├── extensions/          包管理器只写这里（scripts/、skills/），≈ /usr/lib
├── models/  cache/  state/   机器级运行时，≈ /var（账号表、邀请表、用量表、平台状态）
├── data/                【仍在用】机器/共享数据：kb、memes、documents、pictures、
│                        prompts、platforms、persona-avatars、default-kb
└── home/
    ├── shorin/          管理员也在这里
    │   ├── conversation.db      本人会话库（每个成员一份，StoreRegistry 按身份路由）
    │   ├── ledger/  documents/  pictures/  identities/
    │   └── personas/<name>/     私有人格
    └── <friend>/               成员：conversation.db、profile.md、settings.json、workspace/
```

三条规则：用户产生的进 home；管理员发布给所有人的在根、成员只读；机器运行需要的在
state/cache/models。目录名用用户名，账号 id 另存账号表，principal 键用 id 算，改名不掉记忆。

---

## 六、多用户（已落地）

- **账号**：邀请制。管理员在设置页生成一次性邀请码；注册页只收邀请码、用户名、密码。
  第一个账号即超级管理员，所有已有数据归它。首次访问用内置账号 `miyu` / 密码 `miyu` 登录，
  建出管理员后内置账号失效。
- **登录**：WebUI 一律要登录。令牌以 sha256 落盘（`state/web-sessions.json`），30 天有效；
  过期前端回登录页。
- **会话归属**：各人只看自己名下的；管理员看不到成员会话，连开关也没有。每个成员一个
  独立 `conversation.db`，Web / actor / IPC 全路径按会话所属 store 路由。
- **只给管理员的页面**：供应商与 API key、共享人格编辑、扩展与脚本技能管理、QQ 与群管后台、
  共享人格 dashboard、按人拆的用量总表。
- **成员能做**：私有人格、私有 dashboard、表情包库（按 persona scope）、开 dev 会话。

### 工作区
- **成员**：每人一个共享工作区 `home/<user>/workspace`，跨该成员所有会话共用（不看会话记录
  里的 workspace）。
- **管理员**：按会话——会话记录 workspace 有效则用它，否则 daemon 的 cwd；不套沙盒。

---

## 七、沙盒（已落地，Landlock）

> **与计划的出入**：计划明确「不做沙盒，只留三个钩子」；as-built **把沙盒做了**。

- **成员回合**：Landlock 限制。可读写 `home/<user>/workspace`、`/tmp`、`/dev/null`、cache 目录；
  只读 `/usr /bin /sbin /lib /lib64 /etc /proc /sys /dev /run /opt /var` + 脚本目录 +
  当前可执行文件。**沙盒外的读取也禁**——成员只能读工作区和系统目录。
- **管理员**：不套沙盒。
- **进程内守卫**：read / edit / glob / grep / trash / apply_patch / print_image / vision /
  artifact / memes 都在进程内查一遍路径；`rg` 子进程也套沙盒。
- **中转线 CLI 关进沙盒**：成员用 claude-code / codex / antigravity 时，整个 CLI 进程套同一套
  Landlock（`RelayProcess::spawn` → `sandbox::confine_relay`），它起的 Bash/Edit 子进程继承规则；
  只额外放行 CLI 自己的配置目录（`~/.claude`、`~/.claude.json`、`~/.codex`、`~/.gemini`）。
  代价：成员在 CLI 的 Bash 里读得到这些配置文件（含 CLI 登录态），这是这条路径的固有取舍。
- **未覆盖**：Landlock 不管 socket（docker.sock / X11 / dbus）。

---

## 把握与来源

三层代码分层、流水线挂接点、入口清单按 `src/agent`、`src/web`、`src/platforms`、`src/state`、
`src/llm`、`src/config` 的实际模块核对。沙盒行为经隔离 daemon + claude-code/sonnet 真机逐条
验证（工作区可写、`~/.miyu/config` 权限不够、`/etc/hostname` 可读）。目录树按本机
`~/.miyu` 实况列出。多用户与登录态经 testkit/multi-user、testkit/pm 覆盖。
