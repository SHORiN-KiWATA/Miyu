# Antigravity 供应商方案评审(对照 claude-code 线全设计)

日期:2026-09-03。前置调研见 `2026-09-03-antigravity-provider-research.md`。
本文按用户裁定重定方向——**agy 自带工具全开,Miyu 的重复工具从桥上剔除
(原生优先,吃订阅额度),其余 Miyu 工具经 MCP 接入**——把 claude-code 线的每
个部件逐一对到 agy 上,标出「直接复用 / 改参数 / 新写 / 做不到」,重点覆盖终端
与 WebUI 的输出渲染。所有 agy 行为均为本机实测(agy 1.1.24)。

## 0. 本轮新增实测(改变设计的几条)

| 实验 | 结果 | 影响 |
| --- | --- | --- |
| 原生 `run_command` 的 cwd | 默认在 `~/.gemini/antigravity-cli/scratch`,**不是** agy 的 cwd;加 `--add-dir <workspace>` 后 pwd = 工作区 | 拉起参数必须带 `--add-dir`,否则"会话工作区"语义全丢 |
| `tool_info` 形态 | `run_command`:`parameters.CommandLine` + `output`(全文,`\r\n`);`view_file`:`output` 只有 "3 lines, 18 bytes";`grep_search`/`write_to_file`:DONE 无 output;第三态 `ERROR` 带 `error{type,message}` | 只有命令与 MCP 结果有正文;渲染要认 `CommandLine` 键 |
| MCP `tools.eager` | 形状是按工具名做键:`"tools":{"<tool>":{"eager":true}}`;eager 后工具以原生名 `mcp_<server>_<tool>` 出现在步事件里,**不再先 view_file 读 schema** | 桥工具可带真名进卡片,少一跳 |
| 非 eager 的 MCP 调用 | 模型先 `view_file ~/.gemini/antigravity-cli/mcp/<server>/<tool>.json`,再 `call_mcp_tool{ServerName,ToolName,Arguments}` | 卡片名要从 `Arguments` 里拆 |
| 原生 `ask_question`(无头) | `-p` 与 stream-json 输入两种模式都被立即跳过(`step_type: unknown`,0.05s),没有 `WAITING` | 问答只能走桥,且模型会优先挑原生的 |
| `tools:` 白名单列全 55 个原生名 | 整轮 `error_message` 步 + 空响应;二分定位到 `browser_*` 系 | 别用白名单裁单个原生工具 |
| `init` 事件 | 带 `"agent":"<name>"` 字段 | 可校验人格代理是否真的挂上 |
| stderr 噪声 | 每次启动都打 "You are not logged into Antigravity",随后静默鉴权成功 | **不能**拿 stderr 措辞判登录失效 |
| MCP env/cwd 继承 | 子进程继承 agy 进程环境与 cwd(`MIYU_SESSION` 透传实测) | 全局注册一次即可 |

### 0b. 第二批实测(用户追问权限/问答/四重点后)

| 实验 | 结果 | 影响 |
| --- | --- | --- |
| 不带 `--dangerously-skip-permissions` | `permission_mode: request-review`,命令工具 TOOL_ERROR "user denied",**result 仍 SUCCESS 且正文为空**,stderr 提示加 settings.json `permissions.allow` 或用该旗标;`--mode accept-edits` 同样拒命令;`--mode plan` 只写 plan.md 不干活 | 中转必须带该旗标;它就是 agy 版 bypassPermissions;空正文又一种静默失败形状 |
| `--add-dir <ws>` | 工作区三样全生效:AGENTS.md 规则(默认代理回复带 PUMPKIN)、`.agents/agents/*/agent.md`(init.agent 对上)、`.agents/mcp_config.json`(全局为空也能调) | 工作区级配置在无头下**可用**,前提是 add-dir |
| 自定义代理 + 工作区 AGENTS.md | 三次均**未**出现 PUMPKIN(默认代理同环境必现) | 自定义代理不吃工作区规则,人格干净;changelog 说新版默认继承 rules,升级要复验 |
| 自定义代理不写 `tools:` | 只拿到缩水默认集(find_by_name/grep_search/list_dir/view_file/read_url_content/search_web/generate_image/manage_task/schedule/send_message + MCP),**无 run_command/write_to_file/ask_question** | 「原生全开」必须显式列 `tools:` |
| 默认代理的真实工具集(自报) | 20 件:ask_question define_subagent find_by_name generate_image grep_search invoke_subagent list_dir list_resources manage_subagents manage_task read_resource read_url_content replace_file_content run_command schedule search_web send_message view_file write_to_file + MCP;**无 browser_***(init 的 57 是广告清单) | 这 20 件就是白名单蓝本,都在注册表里 |
| 显式 `tools:` 不列 ask_question/generate_image | run_command 正常;问它要问题工具答 NO-QUESTION-TOOL;MCP eager 工具不列也自动带上 | **问答/生图同名双源问题解决**:原生版直接不给,只剩桥版 |
| 两个 `--add-dir`(工作区 + Miyu 私有目录) | 私有目录的代理和 MCP 都加载;但 run_command 的 cwd 在两目录间**随机**(同参数两次不同,模型没传 Cwd) | 多目录方案作废;只能单 `--add-dir <工作区>` |
| 改 agent.md 后新进程 | 立即读到新内容 | 无缓存,按哈希改写安全 |
| MCP 客户端超时 | 睡 75s 的工具默认不超时;`timeoutSeconds` 字段可写 | miyu 条目写 `timeoutSeconds: 1800` 配桥问答 |
| `--conversation` 指向被删/不存在的 id | **不报错**:静默新开会话,init 里 id 变了,stderr 一行 warning,result SUCCESS | 续传丢失判据 = `init.conversation_id != 请求 id`,init 是首行,可立刻杀进程重来 |
| 删 `conversations/<id>.db*` 后模型仍答出旧暗号 | 不是记忆层:`brain/<id>/.system_generated/logs/transcript*.jsonl` 仍在,模型无上下文时用全开原生工具翻遍 ~/.gemini(28 次调用含 ps aux、/proc、读我的 scratchpad)找到 | reset 要连 `brain/<id>` 一起删;「全放行 + 无上下文」会触发文件系统考古,人格与历史重放到位是前提 |
| `agy -p /config` | 有 `trustedWorkspaces`(空)、`toolPermission: request-review`、`permissions` 键 | 信任机制存在但无头下 add-dir 已足够 |

## 1. 工具面:双四档 → agy

claude-code 线(mod.rs:214-259):`tool_capable = scope ∈ {chat, subagent}`;
`native_on` → `--permission-mode`,否则 `--tools ""`;`miyu_on` → `--mcp-config` 内联
JSON + `--allowedTools mcp__miyu` + env `MIYU_MCP_EXCLUDE`(桥侧 mcp_serve.rs:119-140
同时过滤 tools/list 与 tools/call)。

| 项 | claude-code | agy 对应 | 判定 |
| --- | --- | --- | --- |
| native 开 | `--permission-mode bypassPermissions` | `--dangerously-skip-permissions --add-dir <workdir>`;agent.md **显式** `tools:` = 默认代理 20 件减去 `ask_question`/`generate_image`(不写 `tools:` 只有缩水集) | 改参数 |
| native 关 | `--tools ""` | agent.md `tools: []`(实测模型自报 NO-TOOLS,2.2k tok) | 改参数;四档语义可原样保留 |
| 权限模式五选一 | permission_mode 字段 | 无对应(只有全放行 / `--mode plan\|accept-edits` / settings.json 白名单);表单去掉该字段 | 做不到,砍 |
| miyu 开 | `--mcp-config` 每进程 | 全局 `~/.gemini/config/mcp_config.json` 一条 `miyu`,env 走 agy 进程环境继承 | 新写(注册/注销) |
| 去重 | `BRIDGE_DUPLICATE_TOOLS` 六项 | 名单要按 agy 原生名重排(见 §1.1) | 改常量 |
| 环境事实 | `RELAY_ENVIRONMENT_NOTE`/`RELAY_MIYU_TOOLS_NOTE` 追加到 `--system-prompt` | 追加到 agent.md 正文;措辞按 agy 改(§1.2) | 改常量 |
| `--strict-mcp-config` / `--allowedTools` | 有 | 无;全局配置里只有 miyu 一条时等价 | 无需 |

### 1.1 去重名单(agy 版)

agy 原生 57 件(`init.tools`)与 Miyu 工具的重叠判定:

| Miyu 工具 | agy 原生对应 | 处理 | 理由 |
| --- | --- | --- | --- |
| `run_command` | `run_command`(同名!) | **剔** | 原生在训练分布内且吃额度 |
| `web_search` | `search_web` | 剔 | 同上 |
| `web_fetch` | `read_url_content` | 剔 | 同上 |
| `glob` | `find_by_name` | 剔 | 同上 |
| `grep` | `grep_search` | 剔 | 同上 |
| `todowrite` | 无(agy 的 `manage_task` 是后台任务管理) | **不剔** | 与 claude 线不同 |
| `read`/`edit` | `view_file`/`write_to_file`/`replace_file_content` | 不剔 | 同 claude 线:`kb:`/`artifact:` 域原生够不着 |
| `task`/`job`/`alarm` | `manage_task`/`schedule`/`invoke_subagent` | 不剔 | 同 claude 线:agy 每轮一进程,后台活不过回合 |
| `generate_image` | `generate_image`(同名) | 桥版保留,**原生版不进 `tools:` 白名单** | 原生生图落在 agy 自己的产物目录,不进 Miyu 的 tool.image 通道,用户在 WebUI/QQ 看不到;桥版走 bridge_progress 发资产 |
| `ask_question` | `ask_question`(同名) | 桥版保留,**原生版不进 `tools:` 白名单** | 原生版无头必跳;不列它模型就只剩桥版(§5) |
| `vision_analyze`/`print_image`/`use_meme`/记忆/知识库/玄学… | 无 | 不剔 | Miyu 独有 |

同名三件(`run_command`/`generate_image`/`ask_question`)不会同时出现:`run_command` 桥侧剔,
另两件原生侧不进白名单。渲染层剥 `mcp_miyu_` 前缀后就是 Miyu 本名,卡片无歧义。

`every_deduplicated_name_is_a_real_tool` 这条守卫测试原样搬。

### 1.2 环境事实(agy 版措辞要点)

写进 agent.md 正文尾部(常量字节,参与提示词哈希):
- 每轮一进程;`run_command` 后台、`manage_task`、`invoke_subagent`、`schedule` 活不过本轮。
- `mcp_miyu_task`/`mcp_miyu_job`/`mcp_miyu_alarm` 在 daemon 里常驻。
- 仍按「只陈述是什么」写法,不写指令。

### 1.3 代理与桥配置放哪:全局 vs 工作区 vs 私有目录

三条路都实测过:

| 方案 | 人格/桥加载 | run_command cwd | 侵入性 | 判定 |
| --- | --- | --- | --- | --- |
| A. 全局 `~/.gemini/config/agents/miyu` + 全局 mcp_config `miyu` 条目,单 `--add-dir <工作区>` | ✓ | 工作区,确定 | 用户 /agents 面板多一个 miyu 代理;交互式 agy 也挂上 miyu 服务器 | **推荐** |
| B. 写进会话工作区 `.agents/` | ✓ | 工作区,确定 | 往用户仓库里落文件 | 否 |
| C. Miyu 私有目录 + 双 `--add-dir` | ✓ | 两目录随机 | 零侵入 | cwd 不确定,否 |

A 的两个副作用处理:①代理名固定 `miyu`,内容按提示词哈希改写(新进程必重读,已验证),面板里只有一条;②MCP 条目静态写 `"env":{"MIYU_MCP_REQUIRE_SESSION":"1"},"timeoutSeconds":1800`,mcp-serve 见到守卫而没有 `MIYU_SESSION` 就只应答空工具表(现在会降级成无作用域直连,mcp_serve.rs:204-283)。Miyu 拉起 agy 时把 `MIYU_SESSION`/`MIYU_TURN_ORIGIN`/`MIYU_MCP_EXCLUDE`/`MIYU_HOME`/`XDG_RUNTIME_DIR` 放进 agy 进程环境(后两者按「有才给」透传);供应商禁用时删条目、删代理目录。

eager 全开 vs 懒加载:eager 把每件工具 schema 塞进系统提示词(与 claude 线等价);懒加载每件工具首次要多一步 `view_file`,卡片名要从 `call_mcp_tool.Arguments` 拆。建议默认 eager,流解析同时认两种形状。

## 2. 传输与流解析

claude 线:每轮一进程,stdin 一条 user 消息,stdout 逐行 JSON(stream.rs)。agy 同构:

```
agy --print='' --input-format stream-json --output-format stream-json
    --model <m> [--effort <e>] --agent miyu [--conversation <id>]
    --dangerously-skip-permissions --add-dir <workdir> --print-timeout 24h
```

| 事件 | claude 线映射 | agy 映射 | 备注 |
| --- | --- | --- | --- |
| 会话 id | `system/init.session_id` | `init.conversation_id` | 另校验 `init.agent == "miyu"`,不等即报错(否则静默跑在 13.9k 默认提示词上) |
| 正文增量 | `stream_event.content_block_delta.text_delta` | `step_update{step_type:agent_response,state:ACTIVE}.text_delta`;DONE 也可能带最后一片 | 同轮多个 agent_response 步之间补 `\n\n`(镜像 message_start 处理) |
| 思考 | thinking 块 → Reasoning 通道 | 无(只有 `thinking_tokens` 计数) | REPL 没有思考签;计时锚点仍靠 ReasoningStart,行为同 claude 线 |
| 工具开始 | 完整 assistant 帧 tool_use → `RemoteToolStarted{id,name,input}` | `step_update{step_type:tool,state:ACTIVE}` → id=`<step_index>`,name 见 §3,input=`tool_info.parameters` | |
| 工具收口 | user 帧 tool_result → `RemoteToolFinished{id,name,ok,output}` | `state:DONE` → ok=true,output=`tool_info.output` 或空;`state:ERROR` → ok=false,output=`error.message` | 去 `\r` |
| 单次用量 | 最后一次 message_start/delta | 最后一个 `agent_response DONE` 的 `usage` → `last_request_usage` | 上下文表读它 |
| 整轮用量 | result 帧 | `result.usage`(累计) | 字段映射:input→prompt,output→completion,thinking→reasoning,cache_read→cache_read(`cache_reported=true`) |
| 结束 | `result` 帧 `is_error`/subtype | `result.status`:SUCCESS 成功;ERROR 取 `result.error`;WAITING/RUNNING/其它一律当错误 | |
| 静默失败 | 无对应 | ①`error_message` 步 + 空响应 + usage 全 0 ②`init.agent` 不匹配 | 两者都要翻成错误并附 stderr 尾巴 |
| 限流/登录 | `classify_claude_failure` 措辞匹配 stderr+result | 只匹配 `result.error`;**stderr 的 "not logged into" 是启动噪声,不能作 401 依据** | 措辞待真机触发后补 |
| 续传丢失 | "no conversation found"(报错) | **不报错**,静默新开会话;判据 = init 首行的 `conversation_id` ≠ 请求 id → 立刻杀进程、忘映射、全量重放(init 先于模型调用,零浪费) | 新写 |
| 看门狗 | 行间空闲超时杀进程组 | 同;另 `--print-timeout` 要设得比看门狗×工具次数大(默认 5m 太短) | |
| 进程组击杀 | `process_group(0)` + SIGKILL | 同;agy 会再拉 MCP 子进程,组杀能一起带走 | |

`system_message`/`unknown` 步忽略;`user_input` 步忽略。

## 3. 渲染:终端 / WebUI / 平台

claude 线的渲染合同(子代理梳理,锚点略):流层发 `RemoteToolStarted/Finished`
→ reasoning.rs:202-252 翻成 `AgentEvent::ToolCall/ToolResult`,`Bash` 额外发一条
`CommandOutput` → 终端 `is_command_tool(run_command|Bash)` 走 `CommandLiveDisplay`,
其余走 `↳ 主题` 摘要(tool_display.rs 主题表);WebUI 四处 `run_command|Bash` 名单
+ `toolIconName` 家族表;持久化 `ToolFlowRound{remote:true}` 供重绘、三处回放消费
点跳过;平台侧经 EventHub 原样收 tool.started/finished/image。

agy 原生工具名全小写、与 Miyu 命名风格接近,比 claude 的 CamelCase 更贴现有表:

| 渲染点 | 现状 | agy 需要的改动 |
| --- | --- | --- |
| 流层输出整形 | `name == "Bash"` → `truncate_block`,其余 `compact_line`(stream.rs:432) | 改为 `is_command_tool(name)`;`run_command` 输出保换行 |
| reasoning.rs CommandOutput 特例 | `name == "Bash"` | 同上改 `is_command_tool` |
| `is_command_tool` | `run_command \| Bash` | **不用改**(agy 原生就叫 run_command) |
| `command_from_arguments` | 读 `command` 键 | agy 入参是 `CommandLine`;建议在流层把 agy 原生入参**归一化**成 Miyu 键(`CommandLine→command`,`AbsolutePath/TargetFile/SearchPath→path`,`Query→query`,`Pattern→pattern`,`Url→url`),三个渲染端都不用动 |
| `tool_display.rs` 主题表 | 有 claude 原生段(Bash/Read/Edit/WebFetch/Task…) | 新增 agy 段:`view_file`/`write_to_file`/`replace_file_content`/`list_dir`/`find_by_name`/`grep_search`/`read_url_content`/`search_web`/`manage_task`/`invoke_subagent`/`browser_*`/`call_mcp_tool`;归一化后多数能落进既有 `path/query/pattern/url` 分支 |
| `readable_tool_name` | Miyu 名 + claude 名 | 补 agy 名的中英显示名 |
| WebUI `toolSubject` | `args.command \|\| args.cmd` | 归一化后不用改 |
| WebUI `toolIconName` | `search_web` 已在 globe 家族,`generate_image` 已 paintbrush,`run_command` 已 terminal | 补 `view_file`(file-text)/`write_to_file`、`replace_file_content`(square-pen)/`find_by_name`、`grep_search`、`list_dir`(search)/`read_url_content`(globe)/`browser_*`(新图标或 globe)/`manage_task`、`invoke_subagent`(bot)/`call_mcp_tool`(wrench) |
| WebUI 持久化重绘 | `tool_flow` 的 remote 轮照画,`call.ok` 由 `tool error:` 前缀判 | 不用改 |
| 三处回放跳过 | `replay_rounds` 过滤 `remote` | 不用改 |
| 平台(QQ)日志 | 经 EventHub | 不用改 |
| 桥图片/artifact | bridge_progress 在 daemon 侧,与 CLI 无关 | 不用改;`mcp_miyu_generate_image` 照常发 tool.image |
| 桥问答 | bridge_question 走 QuestionBroker,30 分钟超时 | 桥侧不用改;agy 侧 MCP 客户端超时是否可调(`timeoutSeconds` 字段存在)待测,否则 30 分钟等待会被 agy 先掐 |
| 唤醒/shellhook 流 | wake.rs 有 tool.image 分支 | 不用改 |
| `↳` 截断阶梯 | 256→80 主题,120 进度,4000 输出 | 沿用 |
| MCP 名剥前缀 | `mcp__miyu__` | eager:`mcp_miyu_`;懒:`call_mcp_tool` 且 `ServerName=="miyu"` 时取 `ToolName`,input 取 `Arguments`;两种都认 |

`view_file` 等原生工具没有正文只有摘要,终端 Summary 模式本来就不展示输出,
WebUI 卡片展开会是一行 "3 lines, 18 bytes"——可接受。命令实时输出同 claude 线
是数据源硬限制(只在 DONE 给全文),`CommandLiveDisplay` 的尾巴只在收口时刷一次。

## 4. 续传与会话

- 哈希链(session.rs)**整体复用**:键加 provider 维度即可;种子已含系统提示词,
  代理名 `miyu-<hash>` 由同一串提示词派生,所以「提示词变=代理变=新会话=全量重放」
  与现有语义一致,不需要新维度。`host_tools` 维度照留(桥每轮按触发者换脸的坑同样存在)。
- 全量重放 payload(payload.rs)复用:`<conversation-history>` 转写格式不变;差异是
  stdin **只收 text 块**,活体尾巴里的图片也只能降级成占位文本(claude 线活体图片是
  真图)。多模态的补救路线:转写成「附图资产 id,可用 mcp_miyu_vision_analyze 查看」
  ——vision 工具是否接受资产 id 待核。
- 每轮进程的预测前缀 = 已发消息 + assistant 正文,同 claude 线。
- 清空联动:`forget_claude_code_session` 泛化成 `forget_relay_session(provider, miyu_session)`;
  agy 侧转录在 `~/.gemini/antigravity-cli/conversations/<id>.db{,-shm,-wal}`,尽力删。
- 升级路线(第二阶段):agy 的 stdin 多轮已验证可用,可做**每会话常驻进程**,拿到
  claude 线做不到的「步间 followup」;需要进程表(按 Miyu 会话)+ 空闲回收 + 崩溃重开。
  第一阶段仍按每轮一进程,先把闭环跑通。

## 5. 问答(ask_question):已解决

原生 `ask_question` 无头下必被跳过,但显式 `tools:` 白名单里**不列它**即可(实测模型自报
NO-QUESTION-TOOL);桥上的 `mcp_miyu_ask_question` 走 bridge_question 现成流程,agy 侧 MCP
超时用条目的 `timeoutSeconds: 1800` 对齐 30 分钟。`generate_image` 同理不列原生版。剩余
风险只有 agy 升级后默认集/注册表名变动,靠 `every_deduplicated_name_is_a_real_tool` 式的
守卫测试 + init.agent 校验兜底。

## 6. 配置面与 TUI

| 字段 | claude-code | antigravity |
| --- | --- | --- |
| 启用 | 供应商 enabled 总开关 | 同 |
| 显示名 | | 同 |
| binary | 空=PATH `claude` | 空=PATH `agy` |
| native_tools 四档 | | 同(off 走 `tools: []`) |
| miyu_tools 四档 | | 同 |
| 权限模式 | 五选一 | 砍 |
| 看门狗秒 | 300 | 同 |
| — | | 新增 `miyu_tools_eager: bool`(默认 true) |
| — | | 新增 `print_timeout_seconds`(映射 `--print-timeout`) |
| 模型 | 预置四别名,跳过 /models | 预置 §调研 2.1 表;可选增强:`agy models --output-format json` 子进程拉活表 |
| 思考档 | 五档 `--effort` | 三档;gemini 模型名自带档位,`--effort` 对它们是否生效待测,先只对 claude-* 暴露 |
| prefer_subscription | 剥 ANTHROPIC_* | 无对应,砍 |
| timeout_seconds/max_output_bytes | 死配置(委托工具已删) | 不带 |

`claude_code_template` 的「恒存在、置顶、不可删、豁免 base_url 校验、禁用时端点装配
报错」五件套逐一复制;顺序建议 Claude Code 第一、Antigravity 第二。

## 7. 复用/新写清单

| 部件 | 处置 |
| --- | --- |
| session.rs 哈希链 | 泛化到 `llm/openai_compatible/cli_relay/session.rs`,两线共用 |
| payload.rs 转写 | 复用,图片块降级分支加开关 |
| mcp-serve | 加 `MIYU_MCP_REQUIRE_SESSION` 守卫;其余不动 |
| bridge_progress/bridge_question | 不动 |
| RemoteToolStarted/Finished → 卡片车道 | 不动 |
| reasoning.rs Bash 特例、stream.rs 整形 | `== "Bash"` → `is_command_tool` |
| tool_display / readable_tool_name / app.js 图标 | 加 agy 段 |
| 流解析 | **新写**(init/step_update/result + 入参归一化 + 两种 MCP 名形状 + 静默失败判定) |
| agent.md 生成与清理 | **新写**(按哈希落盘、init.agent 校验、孤儿目录回收) |
| 全局 MCP 注册/注销 | **新写**(读改写 `~/.gemini/config/mcp_config.json`,只动 `miyu` 键,保留其它字段) |
| 配置/模板/协议枚举/变体/表单/端点装配 | 镜像 claude-code,约 400 行 |
| 测具 | 假 agy(吐三类事件)+ 真机脚本(续传/桥/卡片) |

## 8. 待用户拍板

1. 默认模型(建议 `gemini-3.8-flash-high`)。
2. 桥工具默认 eager 全开,还是懒加载省 token。
3. 方案 A(全局代理 `miyu` + 全局 MCP 条目带守卫)可接受吗——交互式 agy 会多看到一个 miyu 代理和一个空的 miyu 服务器。
4. `native_tools` 的 off 档还要不要留(用户已说不必关,但四档结构留着成本很低)。
5. 原生 `define_subagent`/`invoke_subagent`/`manage_subagents`/`send_message`/`schedule`/`manage_task`(都活不过本轮)是否也从白名单摘掉。
6. 第一阶段每轮一进程,第二阶段常驻进程——是否认可分两步。

## 9. 上线前实测清单(合并调研 §5)

- ~~AGENTS.md / 工作区 mcp_config 是否加载~~ → add-dir 下加载,自定义代理不吃规则(已测)。
- ~~改 agent.md 后是否重读~~ → 新进程必重读(已测);同一会话跨轮换代理内容未测(方案上不会发生)。
- ~~`--conversation` 不存在 id~~ → 静默新开(已测)。限流/额度/登录过期的 `result.error` 措辞仍未触发。
- `timeoutSeconds` 只验证了可写与 75s 不超时,1800s 未真等。
- `--effort` 对 gemini-*-high/low 模型名是否有效或报错。
- 原生 `generate_image` 产物落点。
- 长会话 conversation_id 是否变;agy 自压缩行为。

## 10. 用户点名的四个重点:提示词 / 会话 / reset / compact

**提示词。** agent.md 正文 = Miyu 系统提示词(人格 + hint + 环境事实),整体替换 agy 默认指令
(13.9k→2.2k 基线,再加白名单工具 schema 约 4–5k)。文件按提示词哈希改写,新进程必重读;哈希
链种子含系统提示词,所以提示词一变就是新会话全量重放——与 claude 线 `--system-prompt` 语义
完全一致,只是载体从命令行参数变成磁盘文件。注入型 hint 的「位置 >> 措辞」经验照用:hint 在
正文尾部、环境事实再往后,都是常量字节。自定义代理不吃工作区 AGENTS.md,人格不被仓库规则污染。

**会话。** `--conversation <id>` 续传,id 从 init 首行拿;哈希链(session.rs)整体复用,键加
provider 维度;每轮进程预测前缀 = 已发消息 + assistant 正文。差异两点:①续传目标丢失不报
错而是静默新开,判据改成 id 比对,init 先于模型调用,可零浪费重来;②agy 侧转录在
`~/.gemini/antigravity-cli/conversations/<id>.db*` **和** `brain/<id>/`(转录日志、步输出、
scratch)两处。

**reset(清空上下文)。** 与 claude 线同一做法:六个清空入口(reset/wipe/平台清空/删会话/
actor 三处)挂 `forget_relay_session(provider, miyu_session)`,丢映射 → 下一轮哈希链必失配 →
新会话。用户说的「清空 = 开新会话」正是现状,agy 没有原地清空,也不需要。多做一步:尽力删
`conversations/<id>.db{,-shm,-wal}` 与 `brain/<id>/`——不删的话旧转录躺在磁盘上,全开的原生
工具在模型缺上下文时会把它翻出来(实测 28 次调用考古出旧暗号)。删不到只记日志。

**compact。** claude 线两层:Miyu 自己的 compact 改写历史 → 链失配 → 新会话 + `<conversation-
history>` 转写重放压缩后的历史;另外 `--autocompact` 把 Miyu 有效窗口交给 claude 让它在会话
内自压缩、id 不变。agy 只有第一层:没有 autocompact 旗标,Miyu compact 后照样新会话重放,行为
正确、成本是一次全量重放(与 claude 线 compact 后一样)。compact 触发所需的单次用量读
`agent_response DONE` 的 `usage`(实测每次调用都带)。agy 自己是否/何时压缩、压缩后 id 会不会
变未知——即便变了也落进「id 不匹配 → 重放」路径,不会丢正确性。Gemini 3.8 窗口 1M,自压缩压力
远小于 claude。

结论:四点里没有做不到的,差异全部落在「检测信号不同」(id 比对代替报错)和「多删一个目录」上。
