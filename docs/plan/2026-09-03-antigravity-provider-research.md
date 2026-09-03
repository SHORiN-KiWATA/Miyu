# Antigravity(agy CLI)特殊供应商可行性调研

日期:2026-09-03。目标:仿照内置的 Claude Code 特殊供应商(`protocol = "claude-code"`),
把 Google Antigravity 的 CLI 伴侣 `agy` 做成 Miyu 的第二个「本机 CLI 中转」供应商。
本文全部结论来自本机实测(agy 1.1.24,antigravity 2.11.0,登录态为个人账号),
测具与原始事件流存放在会话 scratchpad `agyprobe/`(阅后即焚,不进仓库)。

结论先行:**可行,而且形态与 claude-code 高度同构**。agy 的无头模式几乎是照着
`claude -p` 做的(`-p` / `--output-format stream-json` / `--input-format stream-json` /
`--conversation` 续传 / `--effort` / MCP 子命令)。三处关键差异都找到了等价物:

| 需求 | claude 的做法 | agy 的等价物(实测) |
| --- | --- | --- |
| 人格整体替换系统提示词 | `--system-prompt` | 无旗标。用**自定义代理** `~/.gemini/config/agents/<name>/agent.md`,正文即系统提示词,替换默认指令(input 13.9k→2.2k tok) |
| 关掉 CLI 自带工具 | `--tools ""` | agent.md frontmatter `tools: []`(实测模型自报 NO-TOOLS);`tools: [run_command]` 可按名单开 |
| 挂 Miyu 工具桥 | `--mcp-config` 每进程一份 | 只有全局 `~/.gemini/config/mcp_config.json`;但 MCP 子进程**继承 agy 的环境变量与 cwd**,一次注册 `miyu mcp-serve`,靠 `MIYU_SESSION`/`MIYU_HOME` 环境按会话分流 |

## 1. 本机形态

- 包:`antigravity`(IDE 本体,/opt/Antigravity,Electron)与 `antigravity-cli`(唯一二进制 `/usr/bin/agy`,Go 静态链接,209MB)。
- 数据目录:`~/.gemini/antigravity-cli/`(conversations/*.db、log/、builtin/skills、mcp/<server>/<tool>.json 工具 schema 缓存)。
- 配置目录:`~/.gemini/config/`(config.json、mcp_config.json、agents/、projects/)。
- 登录态在 CLI 内部(`agy models` 能拉列表即已登录),Miyu 不经手凭据——与 claude-code 相同。

可用模型(`agy models`):

| 模型 id | 备注 |
| --- | --- |
| gemini-3.8-flash-{high,medium,low} | 档位编码在模型名后缀 |
| gemini-3.7-flash-*、gemini-3.6-flash-* | 同上 |
| gemini-3.1-pro-{high,low} | |
| claude-sonnet-4-6、claude-opus-4-6-thinking | 走 Google 的额度 |
| gpt-oss-120b-medium | |

`--effort low|medium|high` 三档(claude-code 是五档)。

## 2. 协议面实测

### 2.1 调用形态

```
agy --print='' --input-format stream-json --output-format stream-json \
    --model gemini-3.8-flash-low [--effort high] [--agent <name>] \
    [--conversation <id>] --dangerously-skip-permissions [--print-timeout 30m]
```

- `--print` 必须带参数(空串即可),否则它把下一个旗标吃成提示词。
- stdin 每行一条 `{"event":"user","message":{"content":"..."}}`(也接受 `content:[{"type":"text","text":...}]`,**只支持 text 块**,图片进不去)。
- 单进程多轮已验证:两条 user 事件顺序回答,第二轮记得第一轮的暗号(TANGERINE)。关 stdin 即结束。
- 缺 `event` 字段→result status ERROR 退出码 1;未知事件名→stderr 警告后跳过;`control_request`/斜杠命令→退出码 2。

### 2.2 输出事件

```
{"event":"init","conversation_id":"…","init":{"model":"…","cwd":"…","permission_mode":"always-proceed","tools":[57 个]}}
{"event":"step_update","step_update":{"conversation_id","step_index","state":"ACTIVE|DONE","step_type":"user_input|agent_response|tool|system_message","text_delta":"…","usage":{…}}}
{"event":"result","result":{"conversation_id","status":"SUCCESS|ERROR|…","response":"…","error":"…","num_turns","usage":{…}}}
```

- 文本增量:`agent_response` 的 `ACTIVE` 分片带 `text_delta`(claude-sonnet-4-6 实测逐词分片;flash 一次成句)。
- 工具:`step_type: tool` + `tool_name` + `tool_info{name, parameters, output}`,ACTIVE/DONE 成对——可直接映射到 claude-code 线的 `RemoteToolStarted/Finished` 卡片。
- 用量:`step_update.usage` 是**单次调用**;`result.usage` 是**整轮累计**(与 claude 的 result 帧同一个坑,上下文表要读 step 级)。
- 思考:不外露内容,只有 `thinking_tokens` 计数。
- `init.tools` 是静态广告清单(57 个,`tools: []` 的代理也照样列 57),**不能**用来探测工具面。
- `cache_read_tokens` 全程为 0,没观察到提示词缓存生效。

### 2.3 token 与延迟

| 场景 | input tok(单次) | 耗时 |
| --- | --- | --- |
| 默认代理,回 PONG(空目录) | 13,910 | 1.2s(flash-low) |
| 自定义代理,工具默认全开 | 6,797 | 1.8s |
| 自定义代理,`tools: []` | 2,248 | ~1.5s |
| 自定义代理,`tools: [run_command]`,含一次命令往返 | 7,964 | — |
| `--conversation` 续传第二轮 | 14,127(默认代理) | — |
| claude-sonnet-4-6 单句 | 15,936 | **30s** |

默认系统提示词约 11.7k tok(13.9k − 2.2k),自定义代理把它整段换掉;剩下的 ~4.5k 是工具 schema。

### 2.4 自定义代理(人格注入口)

```markdown
---
name: miyu-<hash>
description: …
mainAgent: true
subagent: false
tools: []            # 或 [run_command, view_file, …]
---

# Identity
<Miyu 的系统提示词原文>
```

- 只有全局目录 `~/.gemini/config/agents/<name>/agent.md` 被无头模式识别;工作区 `.agents/agents/` 在测试目录里**没被发现**(日志 `Agent "x" not found, falling back to default`)。
- 正文替换默认指令(公开博客原话:"The markdown body compiles directly into its system prompt")。
- `tools` 白名单里的名字必须是注册表里的组件名:`wait_5_seconds`、`command_status` 都报 `tool "…" not found in registry`,而且**整轮静默失败**——stdout 只有 init 和空 result,usage 全 0,退出码 0,错误只在 stderr/日志。
- `--conversation` 续传时代理随会话保持,不带 `--agent` 也一样。
- 变更代理文件后老会话是否重读:**未测**(设计上可以用「提示词哈希进代理名」绕开:提示词变=新代理=新会话,与现有哈希链语义一致)。
- 其它 frontmatter 字段(changelog/博客):`hidden`、`inheritMcp`、`commandExecutionPolicy`、`model`、`permissionMode`、`skills`。`mcpServers` 键在二进制里存在,是否可写进 agent.md 未测。

### 2.5 MCP 桥

- 全局 `~/.gemini/config/mcp_config.json`(`agy mcp add <name> <cmd> [args]`,支持 `env`、`cwd`、`disabled`、`disabledTools`);文档还说工作区 `.agents/mcp_config.json` 可用,但无头模式在测试目录里没加载(与 2.4 同一现象)。
- 手写 40 行 stdio MCP 服务器实测:模型经元工具 `call_mcp_tool{ServerName, ToolName, Arguments}` 调用,结果原样返回(48213)。
- 调用前模型会先 `view_file ~/.gemini/antigravity-cli/mcp/<server>/<tool>.json` 读 schema——每个新工具多一步文件读取。
- **子进程继承 agy 的环境变量与 cwd**(`MIYU_SESSION=sess-777` 原样到达服务器进程,cwd=agy 的 cwd)。这意味着 Miyu 只需全局注册一次 `miyu mcp-serve`,每轮拉起 agy 时把 `MIYU_SESSION`/`MIYU_HOME`/`XDG_RUNTIME_DIR` 放进 agy 的环境即可——claude-code 线第六轮那个「必须显式透传」的教训直接适用。
- 代价:注册是全局的,用户自己交互式开 agy 也会带上 Miyu 工具(此时没有 `MIYU_SESSION`,桥会 404 或滑进直连兜底)。需要 mcp-serve 在缺会话时干净拒绝。

### 2.6 权限与工作区

- 无头没有交互审批:`--dangerously-skip-permissions` 全放行;`--mode accept-edits|plan`;或 `~/.gemini/antigravity-cli/settings.json` 的 `permissions.allow` 白名单。软拒绝时退出码仍为 0,只在 stderr 提示。
- `--print-timeout` 默认 5 分钟,必须调大。
- 工作区规则(`AGENTS.md`/`GEMINI.md`)在测试目录里没生效(cwd 有 AGENTS.md 要求以 PUMPKIN 结尾,回复没有)。但 changelog 提到「trusting a folder」后才加载工作区 hooks,**信任状态怎么记录没查到**——如果会话工作区恰好是用户在 Antigravity 里信任过的仓库,人格可能被那个仓库的 AGENTS.md 污染。这是上线前必须补测的一项。

### 2.7 错误面

- 结构:`result.status != SUCCESS` + `result.error` 文本;退出码 0/1/2。
- 限流、额度耗尽、登录过期的具体措辞**没触发到**(二进制里只有 proto 名 `QuotaFailure`/`FetchQuotaStatus`,无明文)。设计上先做「非 SUCCESS 一律上抛 + 措辞待补」,与 claude-code 线当年 issue #32 的处理方式相同:先宽后窄。
- 静默失败(2.4)要专门兜:空响应 + usage 全 0 + 退出码 0 → 当错误处理并附 stderr 尾巴。

## 3. Miyu 侧施工方案(镜像 claude-code)

| 部件 | claude-code 现状 | antigravity 对应 |
| --- | --- | --- |
| 配置 | `plugins.claude_code: ClaudeCodePluginConfig`(binary/permission_mode/native_tools/miyu_tools/idle_timeout/prefer_subscription) | `plugins.antigravity`:binary(默认 `agy`)/native_tools 四档/miyu_tools 四档/idle_timeout/print_timeout;无 permission_mode(只有全放行) |
| 供应商模板 | `ProviderConfig::claude_code_template()`,恒存在、默认禁用、预置模型 | `antigravity_template()`,预置 2.1 的模型表,默认模型待拍板(建议 gemini-3.8-flash-high) |
| 协议枚举 | `ProviderProtocol::ClaudeCode`,`provider_uses_claude_code` | `ProviderProtocol::Antigravity` |
| 思考档 | `claude_code_reasoning_variants` 五档→`--effort` | 三档→`--effort`;gemini 模型名自带档位,变体表按模型过滤 |
| TUI 表单 | `config_tui/claude_code_form.rs`(99 行) | 同构一份 |
| 传输 | `llm/openai_compatible/claude_code/{mod,payload,session,stream}.rs`(1,580 行) | `antigravity/` 四件:stream 解析换成 init/step_update/result;payload 沿用 `<conversation-history>` 转写;session 哈希链**可直接泛化复用**(键加 provider 维度) |
| 人格 | `--system-prompt` | 每轮按系统提示词哈希落 `~/.gemini/config/agents/miyu-<hash>/agent.md`(frontmatter 由 native_tools 档位生成),`--agent miyu-<hash>`;定期清理孤儿代理目录 |
| 桥 | `--mcp-config` 临时文件 | 供应商启用时写全局 mcp_config.json 一条 `miyu`;禁用时删;`mcp-serve` 零改动 |
| 工具卡片 | RemoteToolStarted/Finished | 同一条车道,`tool_info.output` 直接有结果文本 |
| 测具 | testkit/claude-code(假 claude + 真机脚本) | 假 agy(吐 init/step_update/result)+ 真机脚本 |

预估规模与 claude-code 线相当(约 2k 行含测试),其中 session/payload/RELAY 说明/桥/卡片车道都是复用,真正新写的是 stream 解析、agent.md 生成与全局 MCP 注册。

## 4. 与 claude-code 线的语义差异(要写进文档的)

1. 系统提示词不是每次进程传参,而是磁盘上的代理文件——提示词变化即新会话(全量重放)。
2. 没有 `--autocompact`:长会话由 agy 自行管理,Miyu 侧 compact 触发哈希失配后全量重放,行为可预期但不能钉窗口。
3. 思考内容不外露;图片进不去(stdin 只收 text 块),多模态只能经桥走 Miyu 的 vision 工具。
4. 用量按 Google 额度/积分,`agy -p /usage` 可查,token 单价没有目录(成本列显示「—」)。
5. 交互式 agy 会共享同一份全局 MCP 注册。

## 5. 上线前必须补的实测

- 信任过的工作区里 AGENTS.md/工作区 mcp_config 是否被无头模式加载(2.6)。
- 改写 agent.md 后老会话是否重读(2.4)。
- 限流/额度/登录过期时的 `result.error` 措辞(2.7)。
- `--print-timeout` 超时与流空闲看门狗的交互;`--conversation` 指向被清理会话时的错误措辞(对应 claude-code 的 `resume_session_lost` 自愈)。
- 长会话(>50 轮)下 agy 自己的压缩是否改变 `conversation_id`。

## 6. 本次探针残留

- 全局 `~/.gemini/config/agents/miyuprobe{,2}` 已删除;`minimcp` 已 `agy mcp remove`;`mcp_config.json` 回到 `{"mcpServers":{}}`。
- `~/.gemini/antigravity-cli/conversations/` 留有 8 个探针会话(PONG/TANGERINE/秘密数字等),无害,可在 agy 的 `/resume` 里删。
