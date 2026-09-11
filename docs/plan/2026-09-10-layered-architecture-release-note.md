# 待并入 next-release-note.md(分层架构重构,worktree 里写不了主检出那份)

## 重要更新
- 脚本头部多了五个键，一个脚本文件就能把自己的完整「清单」说清楚：`Trust: external` 声明这个脚本可以给 QQ 群这类不可信入口用（默认只给属主，以前脚本在 QQ 里一律不可见、也没有办法放行）；`Permission: read-only` 标记它不改东西；`Example:` 给 stub 加载模式一行调用示例；`Hint: <工具>: <句子>` 让脚本在某个工具同时在场时给模型补一句指路话；`Requires: a, b` 要求本回合先调用过前置工具才放行。全部有中文别名，细节见 `docs/scripts/README.md`。
- 脚本可以回传图片了：stdout 里写一行 `MIYU-IMAGE: <路径> | <说明>`，图片会像生图工具那样在终端内联、WebUI 和 QQ 里显示，那一行不进模型看到的输出。

- 十件日用工具从 Rust 搬成了内置 Python 脚本，功能和工具名不变：天气（get_weather）、占卜（divine）、萌娘百科（query_moegirl）、编解码（codec）、科学计算器（scientific_calculator）、游戏兼容性（game_compat）、Fcitx5 wiki、在线 man 手册（online_man）、DeepSeek 状态（query_deepseek_status）、读剪贴板（read_clipboard）。它们现在装在 `/usr/share/miyu/scripts/personas/default/`，你可以直接改、也可以在自己的脚本目录放同名脚本覆盖；不想要哪件就在脚本目录的 `index.json` 里 `disabled` 掉。配置里 `plugins.weather / xuanxue / moegirl / hash_codec / man / calculator` 六个开关随之退场（旧配置文件里留着也没事，会被忽略）；计算器以前默认是关的，现在默认可用。两处已知差异：`codec` 的 blake3 需要装可选的 `python-blake3`，没装时报「不支持的算法」；`query_deepseek_status` 现在按它的参数说明默认附带近期事故列表，`{"include_incidents":false}` 可关。
- 打包：脚本目录里多出的这十个文件随现有的安装循环一起进包，AUR 不用改。

- 人格目录里可以放一个 `persona.toml` 了（`data/personas/<人格>/persona.toml`），声明这个人格开哪些子系统（记忆、技能、语音、人格提醒、情绪）、启用哪些插件（记账、表情包、知识库、脚本……）。不写就是今天的行为：全开。关掉记忆的人格不会再建记忆库、不联想、不写日记，而不是「装了再关」。开发模式（`miyu dev`）就是一个叫 `dev` 的内置人格：什么都不挂，只留核心那十件工具；想给它加东西，写一个 `data/personas/dev/persona.toml` 即可。
- 工具面改由一条流水线装出来，QQ 等不可信入口能看到哪些工具由每件工具自己的清单决定（内置工具在描述 JSON 里写 `"trust": "external"`，脚本在头部写 `Trust: external`），不再是一张硬编码白名单。QQ 会话看到的工具列表逐字节不变，不会掉缓存。

- WebUI 多用户（邀请制）：WebUI 永远要登录。第一次访问用内置账号登录（用户名 `miyu`，密码 `miyu`），登录后先创建管理员账号（用户名默认是家目录名），建完号内置密码即失效；`miyu web` 的 `-p` / `--password-file` 选项随之退场。管理员在控制台「账号」页生成一次性邀请码（默认 7 天有效），朋友在登录页点「注册账号」凭码建号。每个人只看到自己的会话：列表、打开、改名、删除、事件流全部按人分开，管理员也看不到成员的会话。成员看不到设置页与控制台的管理面板（供应商/密钥、记忆库、知识库、表情包、QQ、群管、好感、赞助、脚本、记账），只剩「数据统计」（只有自己的）和「账号」（改显示名/密码、退出登录）。用量统计逐条记账号，管理员的数据统计页多一张「按人拆分」表。成员对话时的记忆按人隔离（日记/联想只看自己的，可写公共层），情绪和好感度仍是全局的。没有沙盒、没有按人额度，防君子不防小人。
- 新成员注册后有一个三步引导：先决定「它是谁」（直接用 Miyu，或创建只属于自己的人格：起名、写设定（可以不写）、传头像和看板图），再勾它能做什么（记忆、管理员放行的插件、逐个勾选的脚本工具；视觉分析、读写文件这类核心工具常开不用勾，往通讯平台外发只有管理员有），最后写「希望它怎么认识你」。自建人格的记忆、技能、脚本都在成员自己的目录里；随时能在账号页里切换、编辑、删除或再建。开了知识库、记账的成员在控制台有自己的知识库/记账/记忆面板，数据都在自己家目录里。管理员在设置页「成员账号」里决定成员能不能自建人格、能勾哪些插件。
- 登录页现在是独立的一页：没登录时看不到侧栏和输入框。注册密码不再限制长度。
- 说明：终端（REPL / shellhook / `miyu session`）里看到的会话是管理员名下的，成员的不出现；迁移到 `home/<用户名>/` 目录的搬家放在下一个版本单独做。


## 独立版本:目录搬家(分层架构阶段 6,建议单独发一版)
- `~/.miyu` 目录树改成仿 Linux 的布局:`personas/`（共享人格,原 `data/personas`）、`extensions/`（已装的 skills/scripts,原 `data/skills`、`data/scripts`）、`home/<用户名>/`（人的东西:`conversation.db`、`profile.md`、`identities/`、`artifacts/`、`documents/`、`pictures/`、`ledger/`、`shares/`,原来散在 `state/` 与 `data/` 下）。`config/`、`state/`、`cache/` 不动。升级后 daemon 第一次启动自动搬,搬之前预检、搬的过程记日志,中断了下次接着搬或原样退回;有别的 daemon 在跑就等它停。
- 管理员的家目录名取 `MIYU_ADMIN_USER`,没有就用系统用户名（比如 `home/shorin/`）,再没有就 `admin`;创建管理员账号时用户名默认也是它。新装直接就是新布局。
- `miyu layout` 看现在是哪种布局和搬家计划;`miyu layout --apply` 立刻搬（daemon 要先停）;`miyu layout --rollback` 搬回老布局并关掉自动搬家,想再搬 `--apply`。
- WebUI 对话现在也带属主档案（以前只有终端带，网页里她不知道你是谁）。有 `user-identity.md` 的用户升级后网页那条前缀缓存会重建一次。
- 「希望 AI 如何认知你」:控制台账号页多了一个档案框,写进自己的 `home/<用户名>/profile.md`。成员对话时她按成员的档案认人,管理员按自己的;QQ 等通讯平台不看档案。成员注册成功会直接打开这一页。
- 导出/导入认识新目录;旧的导出包导入后下次启动自动搬成新布局。
- 成员用私有人格时，表情包库也是自己的一份（按人格分库，控制台有自己的表情包面板）；控制台里记忆/知识库/表情包/记账四个面板只在当前人格开了对应功能时出现。
- 成员的子进程沙盒（Landlock，照搬 dsh 的 landlock-run，零依赖）：成员回合里只能读写自己家里的 `workspace/`（外加 `/tmp`、脚本缓存可写，`/usr` `/etc` 这类系统目录只读），管理员的家和 `~/.miyu` 的配置、数据库读都读不到；子进程靠 Landlock，daemon 进程内的 read/edit/glob/grep/看图工具靠同一份策略的路径守卫。内核没有 Landlock（5.13 以下或被禁）时成员的命令直接拒绝，不会裸奔。管理员不套沙盒。
- 成员各自一份会话库：`home/<用户名>/conversation.db`，artifact 也在各自家里；管理员的会话（含终端、语音、旧数据）仍在 `home/<管理员>/conversation.db`。以前版本里成员建过的会话不搬（那时成员会话在管理员库里靠归属列区分，成员登录后看不到它们了）。
- 未做:`data/personas/default` 改名成 `personas/miyu`;成员的 run_command 仍是宿主 shell(沙盒策略钩子未做,防君子不防小人)。


## 独立版本(可与目录搬家同版):包管理器 `miyu pm`
- 新命令 `miyu pm`(`miyupm` 同):`install <包名|owner/repo[@ref]|GitHub 链接|本地目录>`、`remove`、`upgrade`、`search`、`list`、`tap add|remove|list`。一个包就是一个仓库,根上一份 `miyu-package.toml` 说明带哪些脚本、技能,或者整个是一个人格(提示词 + 启用集 + 头像 + 只给这个人格的脚本/技能)。装前先摊开文件清单确认;装进来的每个文件都记在 `extensions/pm/lock.json`,卸载按它删,升级时 commit 或内容没变就不动;要装的文件被别的包占着会拒。写包的格式见 wiki「扩展指南」。
- 索引(tap)是一个 GitHub 仓库根上的 `index.json`;官方 tap `SHORiN-KiWATA/miyu-packages` 缺省在列,`miyu pm tap add owner/repo` 加第三方。仓库本身还没建,建了就能 `miyu pm search`。
- 打包提示:AUR 包可加一个 `/usr/bin/miyupm -> miyu` 的符号链接。

## 修复
- 成员也能开 dev 会话了（有沙盒兜底）；换人格时挂在 dev 上的会话不动。
- 表情包插件没开（人格没勾、成员没开）时不再注入「本轮必须发表情包」的提醒——以前它只看全局开关，模型会去加载一个不存在的工具。
- 系统提示词的 `<host-environment>` 多了 `harness="Miyu x.y.z"`、`model="供应商/模型"`、`effort="思考档位"` 三个属性（只在属主/成员会话里有；换模型或换档位那一轮前缀缓存会重建一次）。
- WebUI 的 `/reset` `/compact` 这些命令失效、麦克风按钮不见了：页面一打开就去拿命令清单和语音状态，那时还没登录（401），登录后没再拿。现在登录完重拿。
- 编辑工具失败后卡片上还挂着「准备修改」；正在思考的标题一悬停字就消失（流光字的底色被 hover 盖掉）。
- 回合失败时 WebUI 只显示最外层那句「LLM stream failed after emitting output」，查不到原因；现在把原因链一起带出来。
- 成员用 claude-code / codex / antigravity 这类 CLI 供应商时完全没有沙盒：那些 CLI 自带的 Bash/Edit/Read 跑在它们自己的进程里，不经 Miyu。现在沙盒回合里整个 CLI 进程关进 Landlock（它起的 Bash/Edit 子进程继承规则），原生工具照开；CLI 自己的配置目录（`~/.claude`、`~/.claude.json`、`~/.codex`、`~/.gemini`）放行读写，所以 CLI 能登录、能存会话。注意：这意味着成员在 CLI 里跑的 Bash 也读得到这些配置文件（里面有你的登录态）——这是「CLI 关进沙盒」这条路径的固有代价。
- WebUI 的回合（管理员和成员）现在也带 `<host-environment>`（以前只有终端有），成员的沙盒回合里能看到自己的工作区。
- 登录态落盘：以前 daemon 一重启所有人都被登出，cookie 也只有 1 天，手机上隔天打开就得重登；现在服务端和 cookie 都保留 30 天，重启不掉线。登录过期时直接回到登录页（中文提示），不再是发消息时弹一句英文。
- 成员在 AI 输出时再发一条排不进队：排队检查盯着管理员的库看。
- 刷新页面后每条回合的「累计」「每秒 x toks」没了：输出速度样本落库（`generation_tokens` / `generation_ms`），累计按会话顺序求和，刷新后照旧。成员页面的上下文条「累计」也按自己的会话算。
- 系统提示词的 `<host-environment>`：模型池不止一个时全列，各模型档位不一致写 `effort="mixed"`；沙盒回合多 `sandbox="landlock" workspace="…" writable="…" readable="…"`，模型知道自己只能动工作区。
- 一整段正文末尾跟个「(地址)」时整段被下划线包成一个链接：「标题 (地址)」整行成链的规则只认一句话的标题，段落（句中有句号/问号/叹号，或英文句界）只让地址那半截成链。终端同样。
- 手机端（Android Chrome）滑到底再往下拖，输入框底部被推出屏幕、顶部被切：文档高度钉在可视视口上。
- 成员在 WebUI 里 print_image 报「图片加载失败」、artifact 面板打不开 HTML/图表：图片与 artifact 资源一律从管理员的库找，成员的在自己库里，改成按登录者的库取。
- WebUI 流式输出「字会跳」：模型正在输出 `sudo pacman -Syu` 这类行内代码或粗体时，闭合符号没到之前整段按普通文字排，一到就换成代码样式，那一行前后的字全部重排。现在没闭合的反引号、粗体、代码围栏先补上再渲染，回合结束按原文重画。
- 手机上每轮结束整段重建时会闪一下顶部再跳回底部：重建后先同步钉到底，不再画出那一帧。
- 一个会话在跑时，在别的会话里 `/reset` `/compact` `/pop` 被拦「等这一轮跑完」：前端把「有回合在跑」判成了全局，现在只看当前会话（后端本就是按会话预约的）。
- 成员在会话里换模型报「internal server error」：会话模型覆盖写到了管理员的库。
- 走中转线（claude-code / codex / antigravity 这类只能从 MCP 桥拿工具的供应商）的成员，工具目录是管理员的全量工具面（人格没勾记账也列出 ledger），而且每个工具都报「session not found」：桥按会话所在的库与人格算配置了。
- WebUI 里用户脚本的工具标签退回成裸 id（`battery_care`、`gpustoggle`）：显示名表被后登记的 dev/受限工具面整表冲掉，改成合并登记。
- QQ 里她动过工具之后更不容易突然切成「助手播报腔」：那句「工具结果只是工作材料，别因此换语气」的风格锁以前只给终端和 WebUI，现在群聊人格也带上（终端侧提示词一个字节没动，不掉缓存）。
