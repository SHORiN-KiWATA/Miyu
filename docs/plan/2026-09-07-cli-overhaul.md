# 2026-09-07 CLI 体系重做:让 Miyu 能当别的软件的 AI 后端

> 目标(用户 09-07):把 CLI 补成一个合格的、可被程序驱动的接口。像
> `claude -p` 那样,宿主软件能指定会话、模型、上下文窗口、提示词,能清空
> 指定会话,能拿到结构化输出;并且有一个 stdin/stdout 长驻协议模式,宿主
> 起一个进程常驻,往 stdin 写请求、从 stdout 读事件流。

## 0. 现状缺口(09-07 读码结论)

| 能力 | 现状 | 缺口 |
|---|---|---|
| 指定会话 | `--session 名/编号/id`、`-c` 当前会话、默认阅后即焚 ask 会话 | 会话必须已存在;CLI 无法建/删/列会话 |
| 指定模型 | `miyu models` 写会话覆盖或全局池,持久 | 无单次 `--model` |
| 上下文窗口 | 仅 config 按 provider/model | 无单次 `--context-window` |
| 清空会话 | `miyu reset` 只清当前会话 | 不认 `--session`;daemon 的 ResetConversation 本身支持按名/id |
| 提示词 | 系统提示词=人格文件 | 无 `--system-prompt` / `--append-system-prompt`;人格只能 REPL 内切 |
| 模式 | 一次性调用硬编码 normal(`src/cli/mod.rs:154`) | 无 `--mode dev` |
| 输出 | 渲染过的 Markdown 流,进度与正文混在 stdout | 无 json / stream-json;无 session_id、usage、错误原因 |
| stdin | 5 万字符 / 5 秒超时,超了静默截断 | 后端喂长文档会被悄悄砍 |
| 退出码 | 0 / 1 | 分不出超时、取消、用法错 |
| 附件 | 只能剪贴板粘贴 | 无 `--image PATH`;IPC StartTurn 已收图 |
| 长驻 | 无 | 每问一句起一个进程 |

daemon 侧 IPC 已齐:ListSessions / CreateSession / DeleteSession / RenameSession /
ResetConversation / Pop / Compact / SetSessionModels / SetWorkspace / Cancel /
AnswerQuestion / CloseQuestion,StartTurn 已带 session_id 与 cwd。本次主体是
**给 CLI 补壳,并给 StartTurn 加一组「仅本回合生效」的覆盖字段**,不改
daemon 既有语义。

## 1. 设计原则

1. **覆盖不落盘**。`--model` 等单次参数只影响本回合,不写 config、不写会话
   覆盖表。持久改动仍走 `miyu models` / `miyu session`。
2. **两条路一套 schema**。一次性 `miyu ask --output-format stream-json` 和
   `miyu stdio` 长驻模式出站事件同形,宿主只学一次。
3. **stdout 只放正文/协议,进度与日志走 stderr**。json 模式下 stdout 每行一个
   完整 JSON 对象,别的一个字节不漏。
4. **对外 schema 与内部 AgentEvent 解耦**。内部事件随时会改,纯 UI 事件
   (SpinnerTick 等)不出门。对外事件带 `v` 版本号,只增不删。
5. **后端调用默认不污染她**。`--no-memory` 让本回合不写记忆/日记/经历;
   `--tools` 白名单让宿主关掉 shell/QQ 等副作用工具。(教训见
   memory/miyu-memory-poisoning-by-bugs)
6. **缓存友好**。`--append-system-prompt` 追加在人格提示词之后,保住前缀;
   `--system-prompt` 整体替换等于每次冷启动,文档里写明。

## 2. 命令面

### 2.1 一次性对话 `miyu ask` / `miyu "..."`

```
miyu ask [选项] [消息...]
  --session <名|编号|id>     目标会话(已有);与 --create 连用不存在则建
  --create                   --session 指名不存在时新建(名字即会话名)
  -c, --continue             当前会话
  --model <provider/model|裸名|序号>   本回合模型(不落盘)
  --context-window <N>       本回合上下文窗口 token 数
  --system-prompt <文本|@文件>        整体替换系统提示词(掉缓存,慎用)
  --append-system-prompt <文本|@文件> 在人格提示词后追加
  --mode <normal|dev>        agent 模式,默认 normal
  --persona <名>             本回合人格(会话命名空间跟人格走,见 §5 问题)
  --no-memory                本回合不写长期记忆/日记/经历
  --tools <a,b,...>          工具白名单;--no-tools 一个都不给
  --image <PATH>...          附图,可多次
  --file <PATH>...           附文件(走现有附件落盘链路)
  --cwd <DIR>                本回合工作区,默认调用方 cwd
  --output-format <text|json|stream-json>   默认 text
  --quiet                    text 模式下不打进度/工具行
  --timeout <SECS>           超时取消,退出码 124
  --stdin                    显式从 stdin 读正文(并入消息尾部)
```

`--stdout` 保留为 `--output-format text --quiet` 的别名。

stdin 规则:非 tty 时读到 EOF,不再 5 秒超时;上限提到 2M 字符,超限报错
退出码 2 而不是静默截断。

退出码:0 成功;1 回合错误(模型/工具);2 用法或参数错误;3 会话不存在;
124 超时;130 被取消(Ctrl+C / cancel)。

`MIYU_SESSION` 环境变量作为 `--session` 缺省值(工具桥已用这个名字)。

### 2.2 会话管理 `miyu session`

```
miyu session list [--json] [--all]        默认只列用户会话;--all 含 ask/平台
miyu session new <名> [--mode dev] [--json]
miyu session show <名|id> [--json]        含模型覆盖、工作区、轮数、token
miyu session delete <名|id> [--yes]
miyu session rename <名|id> <新名>
miyu session clear <名|id>                 = ResetConversation
miyu session pop <名|id> [N]
miyu session compact <名|id>
miyu session models <名|id> [目标]         = 现 `miyu models` 按会话
miyu session workspace <名|id> [DIR]
```

现有 `miyu reset` / `miyu pop` 改为认 `--session`,行为不变。

### 2.3 长驻协议 `miyu stdio`

一个进程,JSON Lines 进出,多会话多回合并发,stdin EOF 即退出(先取消
进行中回合)。

入站(stdin):

```jsonc
{"type":"message","id":"r1","session":"翻译","create":true,"content":"...",
 "images":["/abs/a.png"],"overrides":{"model":"...","context_window":128000,
 "mode":"dev","append_system_prompt":"...","system_prompt":null,"persona":null,
 "no_memory":true,"tools":["read","kb_search"]},"cwd":"/proj"}
{"type":"answer","id":"r1","question_id":"q1","answer":"..."}
{"type":"cancel","id":"r1"}
{"type":"session","id":"s1","op":"list|new|show|delete|rename|clear|pop|compact","name":"...","args":{}}
{"type":"ping","id":"p1"}
```

出站(stdout):

```jsonc
{"v":1,"type":"ready","daemon":"0.5.0","pid":123}
{"v":1,"type":"started","id":"r1","session_id":"...","turn_id":"..."}
{"v":1,"type":"text","id":"r1","delta":"..."}
{"v":1,"type":"reasoning","id":"r1","delta":"..."}
{"v":1,"type":"tool","id":"r1","phase":"start","call_id":"c1","name":"read","args":{}}
{"v":1,"type":"tool","id":"r1","phase":"end","call_id":"c1","ok":true,"output":"..."}
{"v":1,"type":"image","id":"r1","path":"/abs/out.png","mime":"image/png"}
{"v":1,"type":"question","id":"r1","question_id":"q1","prompt":"...","options":[...]}
{"v":1,"type":"notice","id":"r1","level":"info","message":"..."}
{"v":1,"type":"done","id":"r1","session_id":"...","text":"完整正文","usage":{...},"model":"provider/model","elapsed_ms":1234}
{"v":1,"type":"error","id":"r1","kind":"session_not_found|turn_failed|cancelled|timeout|usage","message":"..."}
{"v":1,"type":"result","id":"s1","ok":true,"data":{...}}
{"v":1,"type":"pong","id":"p1"}
```

`ask --output-format stream-json` 打的就是上面 started…done 那一段。
`--output-format json` 只打一行 `done`(或 `error`)。

## 3. 实现落点(09-07 调研,file:line 为 main 8cb1e264)

### 3.1 事实基础

- **Agent 每回合新建**(`src/web/turns/task.rs:253`),回合任务拿到的是一份
  私有可写的 `AppConfig`(`task.rs:95-96`),开头已有一段 per-turn 改写区
  (`:121-166`,平台 profile 与会话模型覆盖都在这里套)。
- **TurnResourceCache 键 = 整份 config 的 blake3**(`src/runtime/state.rs:146`),
  LRU 8 条。所以覆盖分两类:改 config 的(模型池)取值有限可接受;取值
  无界的(上下文窗口/提示词)不能走 config,要走 Agent 字段。
- **mode 由会话所属人格决定**(`src/web/sessions.rs:438-452`),IPC 与 actor
  两处强制;StartTurn 的 mode 字段是遗留。dev 模式会把记忆库与 skills 整个
  切到保留人格 dev。
- **系统提示词有两个追加口**:`runtime_system_context` 进 system(改了整会话
  冷启一次);`turn_system_context` 挂在当前用户消息之后并化石化
  (`src/agent/turn_loop/stream.rs:66-89`),零缓存代价。替换没有现成口。
- **记忆总开关已有**:`MemoryStore::writes_enabled`,四个写入函数都查;
  Agent 门面 `set_memory_writes_enabled`(`src/agent/setup.rs:270`),目前只
  有平台 profile 用。ask 类一次性会话**照常写记忆**。
- **工具裁剪流水线已有**(`task.rs:203-252`),ToolRegistry 只有 `unregister`
  没有 retain。`AgentTurnControl::new` 也拿 normal/dev 两张表(`:355`)。
- **IPC 一连接一命令**(`src/web/ipc_server.rs:64-72`),回合转发循环硬过滤
  非本 run_id 事件;Cancel/Answer 都是另开连接。帧格式 u32 长度前缀 + JSON,
  PROTOCOL_VERSION 3。
- **run.completed 不带最终正文**,CLI 侧靠 delta 累加(`one_shot.rs:119,360`),
  `generation.superseded` 清空。
- 远端事件泵(`one_shot.rs:353-674`)与本地泵(`live_turn.rs:53-133`)是两张
  独立分发表,加事件两边都要过。

### 3.2 改动清单

**协议(`src/ipc/protocol.rs`)**:StartTurn 加 `#[serde(default)] overrides:
Option<TurnOverrides>`,结构体字段:`models`、`context_window`、
`append_system_prompt`、`system_prompt`、`memory_writes: Option<bool>`、
`tool_allowlist: Option<Vec<String>>`。不 bump 版本(老 daemon 忽略字段;
CLI 侧用 Ready 帧里的 daemon 版本判断是否支持,不支持报错退出码 2)。
`run.completed` 加 `content` 字段。

**daemon 套用(`src/web/turns/task.rs`)**:

| 覆盖 | 落点 | 方式 |
|---|---|---|
| 模型池 | `:149` 前插优先分支 | 改 config(与会话覆盖同路),补 `usable_model_override` 守卫 |
| 上下文窗口 | Agent 新字段 `context_window_override` | `setup.rs:364/369` 短路,不动 config、不动缓存键 |
| 追加提示词 | `:302-307` push 进 `runtime_system_context`(system 侧) | XML 外壳 `<host-instructions>`;宿主每回合传同一段则前缀逐字节稳定。**不走 turn tail**:AGENTS.md §1.4 指令型注入必须在 system 侧、不化石,否则化石重放会让旧指令跨轮错乱 |
| 替换提示词 | Agent 新字段 `system_prompt_override` | `prepare_for_turn`/`refresh_system_prompt` 短路;指纹计算排除 override 免得反复翻转 |
| 不写记忆 | `:308` 前 `set_memory_writes_enabled(false)`,跳过 `:351` organizer,`unregister("remember_fact")` | 复用现成开关 |
| 工具白名单 | `:248` 后按 `tool_names()` 循环 unregister,normal/dev 两张表同裁 | 给 ToolRegistry 加 `retain_named` 小 API |

**CLI 壳(`src/cli/`)**:

- `args.rs`:`MessageArgs` 扩成 `AskArgs`(§2.1 全部参数);新增 `SessionArgs`
  子命令树与 `Stdio` 子命令;`localize.rs` 登记描述与顺序。
- 新模块 `src/cli/output/`:对外事件 schema(`PublicEvent` 枚举,带 `v`)+
  IPC 事件 → PublicEvent 的翻译表(与 `one_shot.rs` 的 IPC→AgentEvent 表并列,
  用同一份 kind 常量)+ text/json/stream-json 三种 sink。`--stdout` 成别名。
- `one_shot.rs`:事件循环抽成「拉帧 → 分发」两层,让 text 渲染与 json sink
  共用;question 在非 tty/json 模式下变 `question` 事件,由 sink 决定回答
  通道(stdio 走 stdin 的 answer,一次性 json 模式直接 CloseQuestion)。
- 新模块 `src/cli/stdio/`:dispatcher。stdin 一行一请求;每条 message 开一条
  UnixStream 跑 StartTurn(daemon 零改动),事件 fan-in 到 stdout 单写者;
  维护 `id ↔ run_id/turn_id/session_id` 表;cancel/answer 另开连接;
  EOF 时对所有在跑 run 发 Cancel 再退出。
- `session` 子命令:全部映射到现有 IPC(ListSessions/CreateSession/…),
  `--json` 直出 AdminResult.data。`reset`/`pop` 认 `--session`。
- stdin 读取:非 tty 读到 EOF,上限 2M 字符,超限报错。
- 退出码:集中在 `src/cli/exit_code.rs`,错误类型映射。

**联动**:`run_command` 注入的环境变量补 `MIYU_TURN_MODE`(顺手修
`tool_cmds.rs:28-33` 那个本地回退推错模式的坑)。

### 3.3 明确不做

- 按回合切 normal/dev(结构性禁止,记忆命名空间会错位)。`--mode` 只在
  建会话时生效(`session new --mode dev`、`ask --create --mode dev`、
  阅后即焚会话)。
- `--persona` 对已有会话生效(会话归属人格,同上)。只在建会话/阅后即焚时
  生效。
- daemon 侧 IPC 多路复用(重写连接处理器,风险最高,收益为零)。
- `--persona`:CreateSession 只能建在 daemon 当前人格(或 dev)名下,回合也
  不按会话记录解析人格;要做得先让 daemon 按会话解析人格。本轮不做。
- `MIYU_SESSION` 环境变量当 `--session` 缺省:run_command 起的脚本里若调
  `miyu ask` 会递归落进正在跑的会话,风险大于便利。不做。
- `--timeout` 在 text 模式:终端渲染那条路的取消要穿 renderer,本轮只在
  json/stream-json/stdio 生效,帮助里写明。

## 4. 验收(09-07 实测记录)

**单测**:`cargo test --lib -- cli::exit_code cli::output cli::turn_request cli::stdio
ipc:: cli::tests::cli_args cli::localize tools::registry` 67/67;整套 `cargo test
--lib` 与 `token_diet_baseline` 结果见会话末报告。测试二进制编译峰值内存
12G 会被 OOM 杀,systemd-run 单元要给 28G。

**黑盒**:`python3 testkit/cli/run.py`(隔离 home + 独立端口 18395 debug daemon
+ 桩 LLM,桩把它看到的请求当正文回来)48/48:

| 组 | 覆盖 |
|---|---|
| 输出 | json 单行 done;stream-json started→usage→text→done,delta 拼起来 == done.text;stdout 只有 JSON |
| 覆盖参数 | --model(含序号、不存在→2)、--system-prompt 替换、--append-system-prompt @文件→`<host-instructions>`、--no-tools/--tools、--context-window→done.context_window、--model 不落盘 |
| 会话 | --session 不存在→3、--create、历史延续 user_count=2、对已有会话传 --mode→2、list/show/clear/rename/new --mode dev/delete、reset --session |
| 记忆 | --no-memory 时 memory.db episodes 行数不变,默认写 +1 |
| stdin | --stdin 12 万字符不截断;25 万→退出码 2 |
| 失败 | --timeout 1→124 + error.kind=timeout;模型 500→1 + turn_failed |
| stdio | ready/pong;并发两回合各自 done 且慢的后到;每事件带 id;overrides 同源;question→answer 往返;cancel→cancelled;session op list/delete;会话不存在;非法请求→usage;EOF 取消在跑回合后退出 |

修过的坑:测试脚本 readline 无超时会永远挂;人格预设对话也是 user 角色,
桩只数带 TK 标记的消息;`pkill -f` 会匹配到自己的 bash;worktree 路径太深
撞 unix socket SUN_LEN,运行目录放 ~/.cache;daemon 单回合正文上限从 20,000
提到 200,000 字符(`runtime::MAX_CONTENT_CHARS`)。

**真宿主 demo**:`testkit/cli/demo_host.py`,视角是「以 Miyu 为后端的翻译
软件」:stdio 常驻、专用会话、每回合同一段追加指令 + no_memory + no_tools +
指定模型,流式收正文。真实供应商:

| 模型 | 三轮结果 | 累计 cache_read |
|---|---|---|
| bigmodel/glm-5.3-flash | 3/3 done,3.3s/2.3s/1.2s,prompt 2038/2066/2099 | 1984 |
| ririxin/deepseek-v4-flash | 第 1 轮上游 429 限流(error 事件带原因),后两轮 done | 2048 |
| deepseek 直连 | HTTP 402 余额不足,error.kind=turn_failed 正确送达 | — |
| 默认池 antigravity(带人格、带工具、no_memory) | done 18.5s,「我是未有,喜欢折腾 Arch……」 | — |

同一段 `--append-system-prompt` 下第二、三轮读到缓存,前缀稳定(AGENTS.md
§1.6 要求)。

## 5. 裁决(用户 09-07)

1. `--mode` / `--persona` 只在建会话时生效(session new、`--create`、阅后即焚);
   对已有会话传了报错。理由:会话归属人格,按回合切会掰断缓存前缀。
2. `--system-prompt`(整体替换)与 `--append-system-prompt`(追加)都做。
3. 对外 schema 自定,形状与 claude stream-json 相近但扁平。
4. 记忆默认照写(与现状一致),`--no-memory` 显式关闭。
