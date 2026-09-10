# 待并入 next-release-note.md(分层架构重构,worktree 里写不了主检出那份)

## 重要更新
- 脚本头部多了五个键，一个脚本文件就能把自己的完整「清单」说清楚：`Trust: external` 声明这个脚本可以给 QQ 群这类不可信入口用（默认只给属主，以前脚本在 QQ 里一律不可见、也没有办法放行）；`Permission: read-only` 标记它不改东西；`Example:` 给 stub 加载模式一行调用示例；`Hint: <工具>: <句子>` 让脚本在某个工具同时在场时给模型补一句指路话；`Requires: a, b` 要求本回合先调用过前置工具才放行。全部有中文别名，细节见 `docs/scripts/README.md`。
- 脚本可以回传图片了：stdout 里写一行 `MIYU-IMAGE: <路径> | <说明>`，图片会像生图工具那样在终端内联、WebUI 和 QQ 里显示，那一行不进模型看到的输出。

- 十件日用工具从 Rust 搬成了内置 Python 脚本，功能和工具名不变：天气（get_weather）、占卜（divine）、萌娘百科（query_moegirl）、编解码（codec）、科学计算器（scientific_calculator）、游戏兼容性（game_compat）、Fcitx5 wiki、在线 man 手册（online_man）、DeepSeek 状态（query_deepseek_status）、读剪贴板（read_clipboard）。它们现在装在 `/usr/share/miyu/scripts/personas/default/`，你可以直接改、也可以在自己的脚本目录放同名脚本覆盖；不想要哪件就在脚本目录的 `index.json` 里 `disabled` 掉。配置里 `plugins.weather / xuanxue / moegirl / hash_codec / man / calculator` 六个开关随之退场（旧配置文件里留着也没事，会被忽略）；计算器以前默认是关的，现在默认可用。两处已知差异：`codec` 的 blake3 需要装可选的 `python-blake3`，没装时报「不支持的算法」；`query_deepseek_status` 现在按它的参数说明默认附带近期事故列表，`{"include_incidents":false}` 可关。
- 打包：脚本目录里多出的这十个文件随现有的安装循环一起进包，AUR 不用改。

- 人格目录里可以放一个 `persona.toml` 了（`data/personas/<人格>/persona.toml`），声明这个人格开哪些子系统（记忆、技能、语音、人格提醒、情绪）、启用哪些插件（记账、表情包、知识库、脚本……）。不写就是今天的行为：全开。关掉记忆的人格不会再建记忆库、不联想、不写日记，而不是「装了再关」。开发模式（`miyu dev`）就是一个叫 `dev` 的内置人格：什么都不挂，只留核心那十件工具；想给它加东西，写一个 `data/personas/dev/persona.toml` 即可。
- 工具面改由一条流水线装出来，QQ 等不可信入口能看到哪些工具由每件工具自己的清单决定（内置工具在描述 JSON 里写 `"trust": "external"`，脚本在头部写 `Trust: external`），不再是一张硬编码白名单。QQ 会话看到的工具列表逐字节不变，不会掉缓存。

## 修复
- QQ 里她动过工具之后更不容易突然切成「助手播报腔」：那句「工具结果只是工作材料，别因此换语气」的风格锁以前只给终端和 WebUI，现在群聊人格也带上（终端侧提示词一个字节没动，不掉缓存）。
