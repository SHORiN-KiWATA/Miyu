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
- [x] 属主档案改由 `profile.md` 注入,只在 Owner/Member 入口;通讯平台不注入(阶段 6:`user_profile_applies(audience, platform_turn)`——WebUI 回合(External 无平台上下文)现在也带档案,以前只有终端带)
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

- [x] 账号表 + 邀请表(migration v34:`accounts`/`invites`,`sessions.owner`);第一个账号=超级管理员;`-p` 起 daemon 即保证有 `admin` 账号且密码等于它(`ensure_bootstrap_admin`);密码 PBKDF2-HMAC-SHA256 自实现(RFC 4231/6070 向量单测)
- [x] 登录令牌记身份(`WebIdentity{account_id,username,display_name,admin}`);只填口令仍是机器级管理员;`/api/auth/register` 凭一次性邀请码(8 位、默认 7 天、只存 sha256、原子消费)建号即登录;`/api/auth/logout`
- [x] 会话表 owner 列;列表/打开/改名/删除/排序/回合/附件全部按 owner 过滤(`require_local_web_session` 带身份、`resolve_turn_session` 带归属键);管理员名下 = 空串(遗留/终端/语音);成员没有全局指针,「当前会话」= 名下最近一条(没有就建);成员建不了 dev 会话
- [x] 成员回合 principal = `web:<账号 id>`,走 `MemoryAccess::principal`;归属从会话记录来(`pinned_for_turn` 填 `usage_account`),不信请求方声明;管理员仍 Privileged
- [x] 用量账本每行带 `acct`;成员 `/api/usage/{stats,details}` 锁自己;管理员默认全部 + `stats.accounts` 按人拆分,`?account=` 可筛;`/api/admin/usage/accounts` 带用户名
- [x] 管理台闸:`/api/dash/*`、`/api/config`(GET/PUT)、思考档位、全局模型、供应商拉模型、语音、QQ 历史、记忆重置、清空统计、账号/邀请码 → `require_admin`;成员 403。前端 `data-admin-only` 隐藏侧栏设置按钮与控制台十个管理面板,成员只剩「数据统计(自己的)」与「账号」
- [x] 事件流按归属过滤(`web/ownership.rs`):`session_id` → 归属;无则 `run_id` → 活跃回合表/`run.started` 反查;两者都没有的全局事件只给管理员。管理员也看不到成员会话的事件
- [x] 账号面板:改显示名/密码、退出登录;管理员生成/作废邀请码、停用/恢复成员、重设密码、30 天按人用量;登录页加用户名 + 「注册账号」表单
- [x] OOBE 三步引导(注册成功即全屏引导,有动画):①人格——直接用 Miyu 或创建自己的(名字/简介/自写设定可留空/头像/看板图,措辞不假定性别);②功能——记忆开关 + 管理员白名单里的插件(`accounts.member_plugins`、`accounts.member_personas`,设置页可改;视觉分析/读写文件/用量是核心常开不做开关,外发 `platform_outreach` 永不给成员;全局脚本逐个勾选 → `PersonaManifest.plugins.scripts` 白名单,注册时过滤);③认知——`home/<user>/profile.md`。激活人格时把成员名下空会话改挂到私有 scope(没有就新建),否则「建了 Eris 还是 Miyu 在答」。成员私有人格 = `home/<user>/personas/<slug>/{persona.md,persona.json,persona.toml,avatar.*,board.*,memory/,skills/,scripts/}`,会话表 scope `home-<user>-<slug>`,回合里 `prompt.private_persona_dir` 覆盖(提示词/清单/记忆/技能/脚本全跟目录,资源缓存键含它);`/api/account/personas*`、`/api/account/active-persona`;账号页人格卡可切换/编辑/删除;头像走 `/api/persona/avatar?scope=`(只给本人)
- [x] `home/<user>/profile.md` 注入:成员回合把 `prompt.user_identity_file` 指到自己的档案(task.rs,只改 Agent 的配置副本,资源缓存键不变);管理员读 `home/<admin>/profile.md`
- [x] 成员沙盒(09-11,`src/tools/sandbox.rs` + `src/web/sandbox_scope.rs`):照搬 dsh `landlock-run`——fork 后 exec 前在子进程里装 Landlock 允许列表(裸 syscall 444/445/446 + no_new_privs,零依赖),规则随 execve 继承;策略挂回合的 task-local(`with_sandbox`),run_command / job / 脚本工具的 spawn 处 `confine`;成员策略 = 可写 {`home/<user>/workspace`、`/tmp`、`/dev/null`、脚本缓存},只读 {/usr /bin /sbin /lib /lib64 /etc /proc /sys /dev /run /opt /var、脚本目录、内置脚本目录、miyu 二进制}(用户拍板:沙盒外的读也禁);进程内工具经 `sandbox::guard_read/guard_write`(read/edit/glob/grep/trash/apply_patch/print_image/看图/artifact/表情包入库),子进程 HOME=工作区;成员工作区固定 `home/<user>/workspace`;内核无 Landlock 失败关闭。测具 `testkit/multi-user/member_tools_probe.py`(桩模型叫全套工具,成员 20 项)。**中转线**(claude-code/codex/agy)在沙盒回合里整个 CLI 进程关进 Landlock(`RelayProcess::spawn` → `sandbox::confine_relay`,成员策略 + CLI 配置目录可写、HOME 不换),原生工具照开、子进程继承(用户拍板:关进沙盒,不是关工具;代价 = 成员 Bash 读得到 CLI 登录态);WebUI 回合(External 非平台)也带 host-environment;登录态 `state/web-sessions.json`(sha256 令牌 + 身份,30 天,与 cookie 同期)。管理员/终端/平台回合不套。信任枚举 Member 仍未做
- [x] 首次访问建管理员(09-11):WebUI 永远要登录;没有管理员账号时内置账号 miyu/miyu 登录 = 机器级管理员(只填密码的登录没有了)且 bootstrap `account.setup_pending`,前端强制走「创建管理员账号」(`POST /api/auth/setup-admin`,建完直接以该账号登录);有了管理员账号只填口令一律 401;`/api/auth/status` 给登录页提示。`miyu web -p/--password-file` 退场(launch 状态的 `password_file` 字段留作读旧文件)
- [x] 测具 `testkit/multi-user/e2e.py`:隔离 daemon + 桩模型,登录/邀请/注册/归属/管理台闸/用量按人/SSE 归属/停用恢复
- 验证:两个账号各开会话互不可见;成员在共享 Miyu 下 recall 只见 public + 自己;dashboard/设置页 403

### 阶段 6 · 迁移(单独版本,只做搬家 + 回滚脚本)

- [x] `state/conversation.db(+wal/shm)`、`data/{ledger,artifacts,documents,pictures,shared→shares,identities}` → `home/<admin>/`;`identities/user-identity.md` → `home/<admin>/profile.md`(`src/paths/home_layout.rs`,复用资源迁移的预检/日志/原子移动;标记 `.home-layout-v1` 内容 = 管理员家目录名,取 `MIYU_ADMIN_USER` → 系统用户名 → `admin`;新装直接是新布局)
- [x] `data/personas` → `personas/`(目录名仍 `default`/`dev`:persona scope 名穿透会话表/记忆库/脚本技能命名空间,改名要先做 scope 别名,另做)
- [x] `data/skills`、`data/scripts` → `extensions/`(config→data 的资源迁移先落定才搬)
- [x] 会话 id 不变;memory.db 不动;`MiyuPaths` 不加字段(六十处测试夹具),按标记现算 `personas_dir/artifacts_dir/ledger_dir/shared_files_dir/profile_file/conversation_db_dir/identities_dir`
- [x] `miyu layout`(干跑计划)/`--apply`/`--rollback`(搬回并写 `.home-layout-off`,同一二进制不再自动搬,`--apply` 撤销);中断靠 journal 接续/回滚
- [x] 导出/导入:registry 新增 home/personas/extensions 单元,`.home-layout-v1` 随档案走
- [x] 按人拆库(成员各自 `home/<user>/conversation.db`,阶段 8):`runtime/stores.rs` `StoreRegistry`——管理员库 + 按账号懒开的成员库(`StateStore::open_member`,artifact 也落成员家里;附件本体/用量账本/账号表仍在 state 与管理员库);web 层按登录身份取库(`for_identity/for_owner`),actor 与事件归属只有会话 id 就 `for_session/owner_of_session`(先查管理员库、再查开过的成员库、最后按账号表开一遍,结果缓存);会话 id 全局唯一(时间戳+随机)所以查到即定。知识库/记账对成员 = `home/<user>/{kb,ledger}`(`AppConfig::member_home_dir`),记忆/知识库/记账/表情包 dashboard 对成员开放(`dash_config_for`;表情包库按人格 scope 分库 `current_persona_library`,只给有私有人格的成员);bootstrap `account.persona.dashboards` 决定成员控制台露哪些面板;工具桥/工具目录(IPC ToolCall/ToolCatalog)按会话所属成员+人格算配置(`session_scoped_config`),否则中转线成员拿到管理员全量工具面且全报 session not found
- [x] 测具:单测 `paths::tests::home`(搬/回滚/冲突零写入/新装/路径解析)、`config::tests::paths::home_layout_marker_redirects_identity_and_persona_paths`;真二进制 `testkit/multi-user/layout_e2e.py`(新装→跑一轮→回滚→老布局起 daemon→再搬→同会话回合还在)
- 验证:隔离 home 上跑迁移前后 tools 指纹、会话列表、记忆召回逐一相等

### 阶段 7 · 包管理器

- [x] `miyu pm install|remove|upgrade|search|list|tap`(别名 add/i/rm/uninstall/up/update/ls;`miyupm` = argv[0] 识别的 shim,打包做符号链接)——`src/pm/mod.rs`、`src/cli/pm_cmds.rs`
- [x] 索引:tap = GitHub 仓库根上 `index.json`(包名 → owner/repo);官方 tap `SHORiN-KiWATA/miyu-packages` 缺省在列(仓库待建);`tap add` 先验索引可读;`install` 也直接吃 `owner/repo[@ref]`、GitHub URL、本地目录
- [x] 锁文件 `extensions/pm/lock.json`:来源/ref/commit(ls-remote → GitHub API)/版本/文件清单/blake3 指纹;`requires-miyu = ">=x.y.z"`;装前摊开文件清单确认(`-y` 跳过);升级按 commit 或指纹判「已是最新」
- [x] 只往 `extensions/`、`personas/`(人格包另写 `data/prompts/<名>.md`、`data/persona-avatars/<名>/`——人格碎片未归位前的妥协)放文件;目标被别的包占着或不是 pm 装的拒(`--force`);装完 `reload_daemon_if_running`;脚本目录变动本就下一回合重扫
- [x] 清单格式 `miyu-package.toml`(`[package]{name,version,description,kind,requires-miyu}` + `[install]{scripts,skills,persona}` 小 glob);tar 解包拒 `..`/绝对路径/链接;单测 `pm::tests` 7 项;真二进制 `testkit/pm/pm_e2e.py`(离线:本地包装/升/冲突/人格包/miyupm/卸)
- [ ] 未做:签名、二进制包、MCP 配置片段、提示词片段;联网路径(search/按名装/tap)无夹具,手工验
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
| WebAuth 单密码 | src/runtime/state.rs | 阶段 5 | 保留:只填口令 = 机器级管理员登录,与账号表并存(`-p` 同步进 admin 账号) |
| `data/persona-avatars` 目录 | paths | 阶段 6 | 头像进人格目录后 |

## 本次(09-10)已随手修的六个 bug

见 `next-release-note.md`;与架构无关,单独提交。
