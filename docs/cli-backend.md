# 把 Miyu 当后端:程序驱动 CLI 与 stdio 协议

> 面向要把 Miyu 嵌进自己软件的人。两种形态:一次性调用(`miyu ask …`,
> 每问一句起一个进程)和长驻协议(`miyu stdio`,一个进程常驻,stdin/stdout
> 走 JSON Lines)。两者出站事件同一套 schema。

## 1. 一次性调用

```bash
miyu ask --output-format json "把这段翻成日语:……"
miyu ask --output-format stream-json --session 翻译 --create "第一句"
cat long.md | miyu ask --output-format json --stdin "总结"
miyu ask --output-format json --model deepseek/deepseek-chat --no-memory --no-tools "……"
```

### 回合选项(根命令与 `ask` 子命令都认)

| 选项 | 说明 |
|---|---|
| `--session <名/编号/id>` | 目标会话。编号是 `miyu session list` 里的序号 |
| `--create` | `--session` 指名不存在时新建(名字即会话名) |
| `-c, --continue` | 用 daemon 当前会话 |
| `--mode normal\|dev` | **只在新建会话时生效**(`--create`、阅后即焚);对已有会话传了退出码 2 |
| `--model <provider/model\|裸名\|序号>` | 本回合模型,**不落盘**。不在配置里退出码 2 |
| `--context-window <N>` | 本回合上下文窗口(token),不落盘 |
| `--system-prompt <文本\|@文件>` | 整体替换系统提示词。顶掉人格、模式提醒与主机环境块;记忆前言与追加段照旧。**每次调用都是缓存冷启动** |
| `--append-system-prompt <文本\|@文件>` | 在人格提示词之后追加 `<host-instructions>` 块(system 侧)。每回合传同一段则前缀逐字节稳定 |
| `--no-memory` | 本回合不写长期记忆/日记/经历,也不给 `remember_fact` 工具 |
| `--tools a,b` / `--no-tools` | 工具白名单 / 一个不给 |
| `--image PATH` | 附图,可多次 |
| `--cwd DIR` | 本回合工作区 |
| `--output-format text\|json\|stream-json` | 见下 |
| `--quiet` | text 模式不打进度/工具行(`--stdout` = text + quiet) |
| `--timeout SECS` | 到点取消,退出码 124(json/stream-json/stdio) |
| `--stdin` | 从 stdin 读正文到 EOF 并入消息尾部;上限 200,000 字符(与 daemon 单回合正文上限同源),超了退出码 2 |

不带任何上述选项时,`miyu ask` 与旧版行为完全一致(阅后即焚会话、终端渲染)。

### 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 回合失败(模型/工具/daemon 错误) |
| 2 | 用法或参数错误 |
| 3 | 会话不存在 |
| 124 | 超时 |
| 130 | 被取消 |

json 模式下错误也以一行 `error` 事件打到 **stdout**(宿主只解析一个流),
stderr 不再复述。

## 2. 输出格式

- `text`(默认):终端渲染,人看的。
- `json`:只打一行终态(`done` 或 `error`)。
- `stream-json`:逐事件一行,最后一行是 `done` 或 `error`。

每行一个 JSON 对象,固定带 `"v": 1`。字段只加不改,类型只加不删。

```jsonc
{"v":1,"type":"started","session_id":"…","run_id":"…","turn_id":"…"}
{"v":1,"type":"text","delta":"…"}
{"v":1,"type":"reasoning","delta":"…"}
{"v":1,"type":"tool","phase":"start","call_id":"t1","name":"read","arguments":{…}}
{"v":1,"type":"tool","phase":"progress","call_id":"t1","name":"read","message":"…"}
{"v":1,"type":"tool","phase":"output","call_id":"t1","name":"run_command","stream":"stdout","output":"…"}
{"v":1,"type":"tool","phase":"end","call_id":"t1","name":"read","ok":true,"output":"…"}
{"v":1,"type":"image","call_id":"t2","name":"generate_image","asset_id":"…","mime":"image/png","alt":"…"}
{"v":1,"type":"question","question_id":"q1","questions":[{"header":"…","question":"…","options":[…]}]}
{"v":1,"type":"notice","level":"info","message":"…"}
{"v":1,"type":"usage","usage":{…},"model":"…","provider_id":"…","estimated":false}
{"v":1,"type":"done","session_id":"…","run_id":"…","text":"完整正文","usage":{…},"usage_estimated":false,
 "model":"…","provider_id":"…","context_tokens":1234,"context_window":128000,"elapsed_ms":2345}
{"v":1,"type":"error","kind":"usage|session_not_found|turn_failed|cancelled|timeout|disconnected","message":"…"}
```

`done.text` 是 daemon 随终态发的最终正文,不用自己拼 delta。一次性调用里
她若提问(`question`),没有回答通道,自动关闭问题让回合继续。

## 3. 会话管理 `miyu session`

```
miyu session list [--json]
miyu session new <名> [--mode dev] [--json]
miyu session show <名|编号|id> [--json]
miyu session delete <目标> [--yes]
miyu session rename <目标> <新名>
miyu session clear <目标>            清空上下文,会话保留
miyu session pop <目标> <N>
miyu session compact <目标>
miyu session models <目标> [模型|default]
miyu session workspace <目标> [DIR] [--clear]
```

`miyu reset --session X` 与 `miyu pop --session X N` 也认会话。`--json`
直出 daemon 的数据形状。

## 4. 长驻协议 `miyu stdio`

```bash
miyu stdio
```

启动后先打一行 `ready`。之后 stdin 每行一个请求,stdout 每行一个事件;
事件带宿主给的 `id` 回指。多条 `message` 可以同时在跑(各自一条 daemon
连接),事件按到达顺序交错,靠 `id` 分。stdin EOF 或 Ctrl+C:取消在跑的
回合(它们各自收到 `error.kind=cancelled`),然后退出。

入站:

```jsonc
{"type":"message","id":"r1","content":"…","session":"翻译","create":true,"mode":"normal",
 "images":["/abs/a.png"],"cwd":"/proj","timeout":60,
 "overrides":{"model":"…","context_window":128000,"system_prompt":"…","append_system_prompt":"…",
              "no_memory":true,"tools":["read","kb"],"no_tools":false}}
{"type":"answer","id":"r1","question_id":"q1","answer":"蓝"}      // 或 ["a","b"](每题一答)、[["a","b"]](多选)
{"type":"cancel","id":"r1"}
{"type":"session","id":"s1","op":"list|new|show|delete|rename|clear|pop|compact",
 "name":"…","target":"…","new_name":"…","mode":"dev","count":3}
{"type":"ping","id":"p1"}
```

出站在 §2 的基础上多三种:

```jsonc
{"v":1,"type":"ready","daemon_pid":123,"build_id":"…"}
{"v":1,"type":"result","id":"s1","ok":true,"data":{…}}     // session op / answer 的结果
{"v":1,"type":"pong","id":"p1"}
```

`message` 的 `session/create/mode/overrides` 语义与命令行完全一致(同一套
解析代码),报错也同一套 `error.kind`。

## 5. 缓存与记忆的提醒

- `--append-system-prompt` 走 system 侧、每请求新组装、不化石:宿主每回合传
  同一段,前缀逐字节稳定,缓存命中不受影响。
- `--system-prompt` 整体替换:整会话前缀跟人格版本不同,每次调用都是冷启动。
  给一个专用会话反复用同一段可以把损失压到一次。
- `--model` 会连带改工具表的加载形态(stub/full 按模型档位),等于一次
  计划内冷启动。
- 记忆默认照写。程序驱动的批量调用请带 `--no-memory`,否则她会把这些当
  「经历」记进公共记忆。

## 6. 明确不支持

- 按回合切 normal/dev、按回合切人格:会话归属人格,记忆库和技能目录跟着走,
  中途切会错位。模式只在建会话时定。
- `--timeout` 在 text 模式无效。
- `MIYU_SESSION` 环境变量不作为 `--session` 缺省(她自己的脚本里调
  `miyu ask` 会递归落进正在跑的会话)。
