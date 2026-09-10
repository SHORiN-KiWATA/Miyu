# 待并入 next-release-note.md(分层架构重构,worktree 里写不了主检出那份)

## 重要更新
- 脚本头部多了五个键，一个脚本文件就能把自己的完整「清单」说清楚：`Trust: external` 声明这个脚本可以给 QQ 群这类不可信入口用（默认只给属主，以前脚本在 QQ 里一律不可见、也没有办法放行）；`Permission: read-only` 标记它不改东西；`Example:` 给 stub 加载模式一行调用示例；`Hint: <工具>: <句子>` 让脚本在某个工具同时在场时给模型补一句指路话；`Requires: a, b` 要求本回合先调用过前置工具才放行。全部有中文别名，细节见 `docs/scripts/README.md`。
- 脚本可以回传图片了：stdout 里写一行 `MIYU-IMAGE: <路径> | <说明>`，图片会像生图工具那样在终端内联、WebUI 和 QQ 里显示，那一行不进模型看到的输出。

- 十件日用工具从 Rust 搬成了内置 Python 脚本，功能和工具名不变：天气（get_weather）、占卜（divine）、萌娘百科（query_moegirl）、编解码（codec）、科学计算器（scientific_calculator）、游戏兼容性（game_compat）、Fcitx5 wiki、在线 man 手册（online_man）、DeepSeek 状态（query_deepseek_status）、读剪贴板（read_clipboard）。它们现在装在 `/usr/share/miyu/scripts/personas/default/`，你可以直接改、也可以在自己的脚本目录放同名脚本覆盖；不想要哪件就在脚本目录的 `index.json` 里 `disabled` 掉。配置里 `plugins.weather / xuanxue / moegirl / hash_codec / man / calculator` 六个开关随之退场（旧配置文件里留着也没事，会被忽略）；计算器以前默认是关的，现在默认可用。两处已知差异：`codec` 的 blake3 需要装可选的 `python-blake3`，没装时报「不支持的算法」；`query_deepseek_status` 现在按它的参数说明默认附带近期事故列表，`{"include_incidents":false}` 可关。
- 打包：脚本目录里多出的这十个文件随现有的安装循环一起进包，AUR 不用改。

- 人格目录里可以放一个 `persona.toml` 了（`data/personas/<人格>/persona.toml`），声明这个人格开哪些子系统（记忆、技能、语音、人格提醒、情绪）、启用哪些插件（记账、表情包、知识库、脚本……）。不写就是今天的行为：全开。关掉记忆的人格不会再建记忆库、不联想、不写日记，而不是「装了再关」。开发模式（`miyu dev`）就是一个叫 `dev` 的内置人格：什么都不挂，只留核心那十件工具；想给它加东西，写一个 `data/personas/dev/persona.toml` 即可。
- 工具面改由一条流水线装出来，QQ 等不可信入口能看到哪些工具由每件工具自己的清单决定（内置工具在描述 JSON 里写 `"trust": "external"`，脚本在头部写 `Trust: external`），不再是一张硬编码白名单。QQ 会话看到的工具列表逐字节不变，不会掉缓存。

- WebUI 多用户（邀请制）：带 `-p` 起 daemon 时自动有一个 `admin` 账号（密码就是 `-p` 的那个；登录页用户名留空、只填密码，仍然是它）。管理员在控制台「账号」页生成一次性邀请码（默认 7 天有效），朋友在登录页点「注册账号」凭码建号。每个人只看到自己的会话：列表、打开、改名、删除、事件流全部按人分开，管理员也看不到成员的会话。成员看不到设置页与控制台的管理面板（供应商/密钥、记忆库、知识库、表情包、QQ、群管、好感、赞助、脚本、记账），只剩「数据统计」（只有自己的）和「账号」（改显示名/密码、退出登录）。用量统计逐条记账号，管理员的数据统计页多一张「按人拆分」表。成员对话时的记忆按人隔离（日记/联想只看自己的，可写公共层），情绪和好感度仍是全局的。没有沙盒、没有按人额度，防君子不防小人。
- 说明：终端（REPL / shellhook / `miyu session`）里看到的会话是管理员名下的，成员的不出现；迁移到 `home/<用户名>/` 目录的搬家放在下一个版本单独做。


## 独立版本:目录搬家(分层架构阶段 6,建议单独发一版)
- `~/.miyu` 目录树改成仿 Linux 的布局:`personas/`（共享人格,原 `data/personas`）、`extensions/`（已装的 skills/scripts,原 `data/skills`、`data/scripts`）、`home/<用户名>/`（人的东西:`conversation.db`、`profile.md`、`identities/`、`artifacts/`、`documents/`、`pictures/`、`ledger/`、`shares/`,原来散在 `state/` 与 `data/` 下）。`config/`、`state/`、`cache/` 不动。升级后 daemon 第一次启动自动搬,搬之前预检、搬的过程记日志,中断了下次接着搬或原样退回;有别的 daemon 在跑就等它停。
- 管理员的家目录名取 `MIYU_ADMIN_USER`,没有就用系统用户名（比如 `home/shorin/`）,再没有就 `admin`;带 `-p` 起的 daemon 建的管理员账号用户名也是它。新装直接就是新布局。
- `miyu layout` 看现在是哪种布局和搬家计划;`miyu layout --apply` 立刻搬（daemon 要先停）;`miyu layout --rollback` 搬回老布局并关掉自动搬家,想再搬 `--apply`。
- WebUI 对话现在也带属主档案（以前只有终端带，网页里她不知道你是谁）。有 `user-identity.md` 的用户升级后网页那条前缀缓存会重建一次。
- 「希望 AI 如何认知你」:控制台账号页多了一个档案框,写进自己的 `home/<用户名>/profile.md`。成员对话时她按成员的档案认人,管理员按自己的;QQ 等通讯平台不看档案。成员注册成功会直接打开这一页。
- 导出/导入认识新目录;旧的导出包导入后下次启动自动搬成新布局。
- 未做:成员各自一份会话库（现在成员的会话仍在管理员那份库里,靠归属列区分）;`data/personas/default` 改名成 `personas/miyu`。


## 独立版本(可与目录搬家同版):包管理器 `miyu pm`
- 新命令 `miyu pm`(`miyupm` 同):`install <包名|owner/repo[@ref]|GitHub 链接|本地目录>`、`remove`、`upgrade`、`search`、`list`、`tap add|remove|list`。一个包就是一个仓库,根上一份 `miyu-package.toml` 说明带哪些脚本、技能,或者整个是一个人格(提示词 + 启用集 + 头像 + 只给这个人格的脚本/技能)。装前先摊开文件清单确认;装进来的每个文件都记在 `extensions/pm/lock.json`,卸载按它删,升级时 commit 或内容没变就不动;要装的文件被别的包占着会拒。写包的格式见 wiki「扩展指南」。
- 索引(tap)是一个 GitHub 仓库根上的 `index.json`;官方 tap `SHORiN-KiWATA/miyu-packages` 缺省在列,`miyu pm tap add owner/repo` 加第三方。仓库本身还没建,建了就能 `miyu pm search`。
- 打包提示:AUR 包可加一个 `/usr/bin/miyupm -> miyu` 的符号链接。

## 修复
- QQ 里她动过工具之后更不容易突然切成「助手播报腔」：那句「工具结果只是工作材料，别因此换语气」的风格锁以前只给终端和 WebUI，现在群聊人格也带上（终端侧提示词一个字节没动，不掉缓存）。
