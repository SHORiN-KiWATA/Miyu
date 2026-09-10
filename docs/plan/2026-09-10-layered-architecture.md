# 分层架构施工计划(2026-09-10)

架构图与讨论记录:https://claude.ai/code/artifact/20ce2a89-cd70-41f4-a25a-36bdb303f2ea

## 定论(已拍板)

- 代码三层:**core**(今天的 dev_registry 那套:回合引擎、核心工具 11 件、模型客户端与池、状态与 daemon)→ **扩展层**(今天 normal 多出的部分,依赖 core,经挂接接口注册)→ **场所层**(入口只声明信任与能力)。
- 配置只有一种:**persona 就是 preset**。一个目录声明提示词、启用哪些子系统与插件。dev 是启用集为空的内置 persona,`miyu dev` 只是切 persona。**mode 概念退场**。
- 扩展按**挂接点**分两种,不按来源分:子系统(挂进流水线多点:记忆、人格提醒、情绪、语音、技能扫描;编译进来)vs 插件(只往工具面加:内置 ledger/kb/memes/alarm… 与外装 scripts/skills/MCP/插件包;persona 看不出区别)。
- 场所只声明两属性:信任(Owner / Member / External / Internal)+ 能力(能弹问题、浏览器、LaTeX、图、语音)。信任解析顺带产出 principal,随会话冻结;跨端进同一会话不重算工具面,用不了的工具报「此入口不可用」。
- 系统提示词受众五块归位:style-lock、语音协议回人格路径;属主档案变成 `home/<user>/profile.md`,只在属主类入口注入,**通讯平台不生效**;host-environment 删;只剩 LaTeX 一句按场所能力位。
- 子系统属于 persona,场所只决定露不露。情绪/好感度只在通讯平台层生效,保持全局。
- 运行时按启用集**决定构造什么**,不是装了再关(今天 dev_scoped 是后者)。
- 目录仿 Linux:根=系统(config、personas/、extensions/、state|cache|models),`home/<username>/`=人。三条规则:用户的进 home;管理员发布的在根只读;机器运行的在 state。用量表例外放 state 加 account 列。**做一次性迁移**,管理员也进 home。
- 多用户:邀请制(管理员设置页生成一次性码);OOBE 两步(设置专属 AI 人格含头像/看板图;希望 AI 如何认知你);管理员白名单限定成员可启用的扩展;管理员看不到成员会话;不做沙盒/路径根/按人额度(防君子不防小人),留三个钩子。
- Miyu 本身是管理员发布的共享 persona,任何人任何场所可不用。共享 persona 记忆三层可见性(已存在);私有 persona 自己一个库不分层。
- QQ 接入只有管理员;第一版不做 QQ 号绑定账号。

## 施工顺序与 todo

每阶段单独 worktree、单独合并;阶段内先造测具再动代码;**可删项攒清单,等拍板再删,修 bug 与加功能的提交不夹带删除**。

### 阶段 0 · 文档与格式(不动代码)

- [ ] 定死两条边界规则(挂接点分类;子系统属 persona)写进 `docs/`
- [ ] `persona.toml` 格式:`[subsystems]` 逐项开关、`[plugins] enabled = [...]`、可选 `avatar/banner`
- [ ] 扩展清单格式:脚本头部与插件包 manifest 共用五个字段(trust 位、分组、指路句、跨工具闸、附件投递)
- [ ] 场所声明格式:每个入口一行(信任、能力)
- [ ] 目录树与迁移映射表(旧路径 → 新路径,含 personas/default → personas/miyu)
- [ ] 可删项清单初版(见文末)

### 阶段 1 · 扩展清单五字段(纯加能力)

- [x] `src/tools/scripts/header.rs`:头部解析新增 `Trust`、`Permission`、`Example`、`Hint`、`Requires`(4d3837d6;分组本就有)
- [x] 分组:脚本头部 `Group` 与 ToolSpec::groups 本就是数据驱动,内置分组表 groups.json 保持;无需改
- [x] `src/tools/cross_hints.rs` 除内置表外读清单自带指路句(ToolSpec::cross_hints)
- [x] guard 层:`requires_prior_guard` 按 ToolSpec::requires_prior 放行/拒绝;AUR 的「不同轮」互斥语义不同,保留原 guard
- [x] 脚本 stdout `MIYU-IMAGE: 路径 | 说明` 行交给投递层(run_script 拿 ToolProgress)
- [x] 单测 6 组(头部/entry→spec/注册表范围/守卫/指路句/图片回传);受限平台注册表按 Trust: external 收脚本
- 验证:全部内置工具行为逐字节不变(tools 数组指纹对比,见 AGENTS.md 1.6)

### 阶段 2 · 系统提示词归位(独立可做,直接减跨端分叉)

- [x] style-lock 给外部受众(追加末尾,属主字节序不变,Internal 不加);VOICE_PROTOCOL 留到阶段 4 按「可播报」能力位决定
- [ ] 属主档案改由 `profile.md` 注入,只在 Owner/Member 入口;通讯平台不注入
- [ ] 删除 host-environment 一行(可删项,需确认)
- [ ] LaTeX 一句改由场所能力位决定
- 验证:REPL 与 WebUI 同会话跨端 cache_read 不再掉(cache-usage.jsonl 取证法)

### 阶段 3 · 迁纯脚本 13 件

- [x] 十件写成内置脚本 `src/scripts/personas/default/{get_weather,divine,query_moegirl,codec,scientific_calculator,game_compat,fcitx5_input_method_wiki_qurey,online_man,query_deepseek_status,read_clipboard}`(protondb/caniplayonlinux/awacy 合为 game_compat;exchange_rate 留 Rust——记账模块内部调用 fetch_rate)
- [x] 描述/schema 原样搬进头部(首句 >60 字符的三件改写首句;calculator 的 expression 补了 description)
- [x] 内置 Rust 实现退场(用户 09-10 确认):12 个模块、10 份 JSON、`plugins.{weather,xuanxue,moegirl,hash_codec,man,calculator}`、配置 TUI 与设置页对应项
- 验证:子代理逐件真跑(见各脚本报告);受限注册表按 Trust 收脚本有单测;沙箱 daemon `miyu tool-call` 逐件调用见下

### 阶段 4 · persona 启用集 + 三表合一 + 场所两属性

- [x] `config::PersonaManifest`(persona.toml:subsystems/plugins,缺省 all,dev 缺省 core_only);记忆按清单构造(setup.rs / parallel.rs 人格提醒同裁决)
- [x] `tools::compose_registry(config, paths, manifest, surface)`:core → 按清单注册扩展 → 场所按 trust 筛 + ask_question;三个旧名保留为薄包装;形状夹具证明 dev/受限逐字节不变,normal 多 load_skill/manage_skill
- [~] `AgentMode` 退场(用户已批准):工具面与记忆已不再看它(persona 清单裁决),它只剩「哪个 persona」的派生标签(Dev = 保留人格 dev);298 处引用(setup/prompt/context/control/cli/web/footer)的机械替换是独立的一小步,单独提交
- [~] `tools::Surface { trust, interactive_questions }` 已定;各入口今天仍经 build_tool_registry(mode, interactive)/restricted 包装进入,逐入口改成直接声明 Surface 是下一小步
- [ ] 会话创建时快照 persona 指针与场所属性;跨端进入不重算工具面;不可用工具报错文案
- [x] `ask_question` 在 compose 里按 surface.interactive_questions 注册
- 验证:persona-ab 测具(人格遵循度不降);tools 数组指纹;dev 会话记忆确实不构造(无 memory.db 打开)

### 阶段 5 · 多用户(可与阶段 4 并行,只依赖「人格是会话属性」)

- [ ] 账号表 + 邀请表(state 库);第一个账号=超级管理员;现有共享密码升级为其密码
- [ ] 登录 cookie 记账号 id;principal = 哈希(web, daemon 实例, 账号 id),复用 `platform_types::stable_key`
- [ ] 会话表加 owner 列;列表/打开/删除按 owner 过滤;管理员默认只看自己的
- [ ] WebUI 回合带 principal:成员走 `MemoryAccess::principal`,管理员 Privileged(复用 QQ 路径)
- [ ] 用量表加 account 列;成员看自己,管理员看按人拆总表
- [ ] 设置页门槛:供应商/key、共享人格编辑、扩展与脚本技能管理、QQ 与群管后台、共享人格 dashboard、总表 → 管理员
- [ ] OOBE:邀请码 → 账号密码 → 人格(名字/模板/自写/头像/看板图)→ 白名单勾扩展 → 开聊;「直接用 Miyu」入口
- [ ] `home/<user>/profile.md` 注入(属主类入口)
- [ ] 三个钩子:信任枚举 Member(今天=Owner)、回合上下文必填 principal、run_command spawn 处沙盒策略参数(默认完全放开)
- 验证:两个账号各开会话互不可见;成员在共享 Miyu 下 recall 只见 public + 自己;dashboard/设置页 403

### 阶段 6 · 迁移(单独版本,只做搬家 + 回滚脚本)

- [ ] `data/{conversation.db,ledger,artifacts,documents,pictures,identities}` → `home/shorin/`
- [ ] `data/personas/default` → `personas/miyu`;`dev` 同级
- [ ] `config/skills`、`data/scripts` → `extensions/`
- [ ] 会话 id 不变;memory.db 内 origin_session_id 不动
- [ ] 回滚脚本 + 干跑模式 + 备份
- 验证:隔离 home 上跑迁移前后 tools 指纹、会话列表、记忆召回逐一相等

### 阶段 7 · 包管理器

- [ ] `miyu pm install|remove|upgrade|search|list`(一套主语法 + 少量别名;`miyupm` shim)
- [ ] 索引:官方 tap 仓库映射包名 → owner/repo;允许第三方 tap
- [ ] 锁文件:commit sha + 内容指纹;`requires miyu >= x.y`;装前摊开清单
- [ ] 只往 `extensions/` 与 `personas/` 放文件;装完重扫指纹、通知 daemon
- 验证:装/卸/升一个脚本包、一个 persona 包;指纹重扫生效

## 可删项清单(草稿,全部需确认后再删)

| 项 | 位置 | 何时可删 | 谁可能还在用 |
|---|---|---|---|
| `plugins.{weather,xuanxue,moegirl,hash_codec}` 开关字段 | src/config/tool_plugins.rs | 阶段 3 | 零处读取,配置 TUI 不显示 |
| 13 件纯脚本工具的 Rust 实现与 descriptions JSON | src/tools/*.rs | 阶段 3 | restricted 注册表、load_tools 分组表 |
| `dev_registry` / `restricted_platform_registry` | src/tools/mod.rs | 阶段 4 | platforms/mod.rs、runtime/state.rs 缓存 |
| `AgentMode` 及全部分支 | src/agent、src/cli、src/web | 阶段 4 | REPL 模式选择、footer 显示、dev-prompt.md 读取 |
| `with_host_environment` 里的 host-environment 行 | src/agent/prompt.rs | 阶段 2 | 测试 host_environment_is_byte_stable_* |
| `config/identities/user-identity.md` 路径链 | src/config/persona_paths.rs | 阶段 6 | 迁移脚本要先搬 |
| WebAuth 单密码 | src/runtime/state.rs | 阶段 5 | 升级为管理员密码后退场 |
| `data/persona-avatars` 目录 | paths | 阶段 6 | 头像进人格目录后 |

## 本次(09-10)已随手修的六个 bug

见 `next-release-note.md`;与架构无关,单独提交。
