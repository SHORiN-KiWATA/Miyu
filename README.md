<p align="center">
  <img src="pics/miyu-logo.png" alt="Miyu" width="180">
</p>

# Miyu

一个活在终端里的二次元少女。开箱即用的开源 AI 助手，支持接入通讯平台。

>暂时

## 谁是 Miyu？

Miyu 是从我曾经很喜欢的动画中的角色身上汲取灵感制作的虚构角色。

## 有什么功能？

`miyu` 由大模型驱动，默认接入了 [opencode](https://github.com/anomalyco/opencode) 的公共模型服务，你也可以配置自己的大模型服务。除了 Coding，她还可以完成聊天日常、游戏娱乐、系统排障、天气查询、汇率换算、二手市场行情查询等日用场景。

`miyu` 可以与 `fish`、`zsh`、`bash` 集成，终端打字直接无缝对话！

![](./pics/shell-init.png)

有终端交互模式

![](./pics/REPL.png)

自带了 TUI 方便修改配置。

```
miyu config
```

![](./pics/tui.png)

还有 WebUI 

![](./pics/webui.png)

还可以通过 NapCat 接入 QQ，远程操作电脑；亦或是加入群聊，陪网友吹水，帮助你管理群聊。

![](./pics/qq私聊.png)

![](./pics/qq群聊管理.png)


## 如何安装？

- Windows (原生支持 Windows 10 / 11)

  ![](./pics/windows-terminal.png)

  支持单文件绿色部署或源码编译，完整说明请参阅 **[Windows 专属指南与使用文档](README_WINDOWS.md)**：
  ```powershell
  # 源码编译安装
  cargo install --path .
  ```

- Arch Linux

  ```
  yay -S miyu
  ```

- 从源码构建 (Linux / macOS)

  ```
  git clone https://github.com/SHORiN-KiWATA/Miyu.git
  cd Miyu
  cargo build --release
  ```

安装完成后可以运行 `miyu init` 初始化配置和状态文件；也可以直接运行 `miyu daemon start`，首次启动会自动初始化。

## 如何使用？

> 与 `miyu` 运行最适配的是 `kitty`终端

- REPL TUI 交互模式

  ```
  miyu
  ```

- webui 局域网网页

  ```
  miyu web
  ```

- shell hook 终端集成

  最好的集成效果要求使用 `fish`，`zsh` 和`bash` 只能做到单行对话，`fish` 可以完整无缝集成。
  
  ```
  miyu fish-init
  ```

### 会话的三条车道

`miyu` 把「对话落在哪」分成三条互不干扰的车道，不用手动切会话就能随口问问题：

| 入口                                                            | 落在哪                                               |
| --------------------------------------------------------------- | ---------------------------------------------------- |
| `miyu ask <消息>`、`miyu '<消息>'`、管道输入                    | **一次性对话**：临时会话，答完即删，不进任何上下文   |
| shell hook（终端里直接说自然语言）、`miyu new` / `miyu session` | **终端会话**：一直沿用，直到你主动切换               |
| `miyu` 进 REPL、REPL 内的 `/new` 和 `/session`                  | **REPL 会话**：REPL 自己记着上次在哪，重开就回到那里 |

于是：shell hook 正在长篇回复时，另开一个终端 `miyu '顺手问一句'` 不会污染那条对话；REPL 里 `/new` 开的新会话，退出重开还在，而 shell hook 仍留在原来的终端会话上。

一次性入口想例外地发进终端会话时加 `-c`：

```
miyu -c '记住我在写 Miyu 的会话模块'
```

`--session <名称|编号>` 则指定任意一个会话，同样只作用于本次命令。

删除会话不必先切过去：`miyu session` 或 REPL 的 `/session` 弹出菜单后，在目标行按 `Ctrl+D` 确认即可，删完菜单会刷新并留在原处。

### 搬到另一台机器

`miyu export` 把当前安装打成一个 `.tar.gz`（权限 0600），`miyu import` 在新机器上还原：

```bash
miyu export                      # 配置、会话历史、记忆、知识库原文、用户资源
miyu export --index --platforms  # 额外带上向量索引与平台聊天历史
miyu export --no-secrets         # 清空 API key 与令牌，导入后自行补填
miyu export --dry-run            # 只看清单与体积，不写文件

miyu daemon stop                 # daemon 占着数据库，导入前必须停
miyu import miyu-export-*.tar.gz
```

默认**不含**知识库向量索引（很大，且 `miyu kb embed` 可重建）、缓存、日志和其他一次性的本机状态。密钥默认带上并在导出时警告——归档是明文的，别随手发出去。

目标目录已有配置或会话历史时导入会被拒绝；`--force` 会先把现有安装导出成 `miyu-backup-<时间>.tar.gz` 再覆盖。导入后按提示重装 shell 集成、跑 `miyu kb reindex`（知识库记的是旧机器的绝对路径），未带 `--index` 时再跑一次 `miyu kb embed`。

### 重要配置调整

运行 `miyu config` 命令打开配置 TUI。

- 供应商和模型

  `miyu` 默认使用 opencode 的公共 API，推荐配置自己的 API。

- 自定义提示词

  `miyu`的默认提示词是无法修改的。你可以在`自定义提示词`中新建属于自己的 AI 人格，还可以配置 `用户身份` 让对话更加沉浸。 

### 用户资源与 Skill

Miyu 将配置与用户资源分开保存：`~/.miyu/config` 存放 `config.jsonc`、主题和 shell 集成；`~/.miyu/data` 存放 prompts、identities、persona-avatars、scripts 和 skills；运行状态与 Skill 草稿位于 `~/.miyu/state`。

### 内置插件

<details><summary>[展开/收起] 具体介绍</summary>
<br>

- 表情包
  
  表情包毫无疑问是聊天时最重要的部分，在对话时，Miyu 会根据情景自主发送符合情境的表情包。除了自主发送，设置里还可以设置概率、置信度和冷却时间。

  ![](./pics/nvidiafuckyou.png)

  Miyu 自带了一些表情，存放在`/usr/share/miyu`，对应的用户空间目录位于`~/.miyu/data`。表情库是跟随人格的，如果你在设置里新建了自己的人格，那么就无法使用 Miyu 的默认表情。你可以准备一些图片，把路径给 Ai，让其保存到表情库。届时会自动调用识图模型对图片进行分析并保存。Miyu 默认使用 opencode 公共模型服务中的多模态模型进行识图，所以即使不配置自己的多模态模型也可以看图片。

- 玄学算命

  >心理学。
  
  算命就像看天气预报一般稀松平常。Miyu 自带了周易六十四卦、吉凶占、塔罗牌抽取等玄学功能。

  ![](./pics/玄学.png)

  ![](./pics/吉凶占.png)

- 投骰子

  >赌！

  闲来无事可以和 AI 比比大小。

  ![](./pics/骰子.png)

- 闹钟

  >要我说，这比GNOME时钟的闹钟好用多了
  
  Miyu 自带了闹钟，日常泡泡面、番茄钟学习、计时任务什么的都很实用。内置了闹钟音频，你还可以通过路径传入你想要在到点后播放的“闹钟”。

  ![](./pics/set_alarm.png)

- 知识库

  Miyu 自带了 [ShorinWiki](https://github.com/SHORiN-KiWATA/Shorin-ArchLinux-Guide) 中的内容和一些日用 Linux 会遇到的问题作为默认知识库。

  当然，你也可以通过 `miyu kb` 命令，或者通过跟 AI 的自然语言交互管理属于你自己的知识库。

  ![](./pics/kb.png)

- ProtonDB 查询

  可以查询 ProtonDB 上的游戏信息和相应的评论，为 Linux 玩游戏提供参考建议。

- Linux 游戏兼容性调查

  >这个游戏 Linux 能玩吗？

  这是桌面端使用 Linux 的日经问题，Miyu 会去 [ProtonDB](https://www.protondb.com/)、[Are We Anti-Cheat Yet?](https://areweanticheatyet.com/)、[Can I Play On Linux](https://caniplayonlinux.com/)等 Linux游戏兼容性资讯网站获取主要信息，辅以社区玩家的声音，综合判断一款游戏的兼容性并提出建议和注意事项。

  ![](./pics/gaming.png)

- 网络搜索

  即使不配置网络搜索 API，Miyu 也仍然拥有基础的网络搜索和网页读取能力：未配置任何搜索服务时会优先使用 Exa 的免 key 公共额度（每日限量，报错或超额后自动冷却并回退到内置爬虫搜索）。可以在插件配置中设置 Tavily、Firecrawl 、AnySearch、Exa、SearXNG 等网络搜索 API 以获得更佳的搜索效果。

  ![](./pics/web-search-config.png)

- 搜图

  Miyu 还能帮你找图片喔！搜图会根据网络环境并行使用多个来源，并通过视觉模型筛选相关且安全的结果。图片会默认保存至`~/.miyu/data/pictures/web-images`。

  >NSFW 禁止！

  ![](./pics/搜图.png)

- 生图

  支持 OpenAI 的画图服务喔。图片会默认保存至`~/.miyu/data/pictures/generated-images`。

  >这个功能默认用不了，要自己在插件设置里开启并配置 API

  ![](./pics/生图.png)

- 天气查询

  查询天气是每天的必做活动，当然少不了。

  ![](./pics/weather.png)

- 汇率查询

  国际社会，查个汇率也很合理吧？

  ![](./pics/汇率.png)

- Man 手册查询

  >Man！

  专门的手册查询工具，虽然网络搜索也能做到，但这值得做成单独的插件。
  
  ![](./pics/man.png)

- Arch Linux相关

  Arch Linux 是桌面 Linux 的热门之选，Miyu 有一系列插件可以帮助提高 Arch Linux 的日用体验。

  - AUR 状态查询

    >AUR 还在被 DDos 吗！

    AUR 的状态是日用 Arch 时的重要信息之一，不访问网站就能查询的话，在 AUR 安装出现异常时查起来会方便很多。

    ![](./pics/aur-status.png)

  - AUR 包查询

    可以查询 AUR 上的包的具体信息

  - Arch Wiki 查询

    作为 “Linux 圣经”，查询 Arch Wiki 不仅能提高日用 Arch 的体验，对其他发行版也大有裨益。

    ![](./pics/archwiki.png)

  - PKGBUILD 审查

    AUR 投毒的事件搞得人心惶惶，但现在，Miyu 可以帮忙审查 PKGBUILD 啦！

    ![](./pics/pkgbuild审核.png)

- 文件操作

  >自不必说。

  Miyu 支持读写文件、搜索内容、查找文件、删除文件等。

- 计算器和哈希编解码

  为了计算结果的准确性，Miyu 自带了科学计算器和哈希编解码的能力。

  ![](./pics/hash.png)

- 记忆系统

  Miyu 的记忆分为短期日记、长期日记和知识点。每个成功完成的对话轮次会立即写入短期日记；同一人格累计 14 条未整理日记后，由独立后台线程并行提炼长期知识点和有回溯价值的长期经历，不会阻塞正常回复。成功整理的短期日记默认保留 14 天，每次有效联想会刷新保留时间；召回达到 3 次时会立即进入长期化整理。尚未成功整理的原文超期后会退出自动联想但不会丢失，后台仍可继续整理；整理成功后再物理清理。已经长期化的日记不再刷新短期原文的清理时间。

  联想会同时检索三类记忆，并使用 `jieba-rs` 中文分词进行低成本匹配。Embedding 后续可以作为可选辅助接入，但不是记忆系统运行的前提。长期知识点和长期日记会随时间衰减为“已遗忘”，不物理删除；显式搜索仍可找回。

  `/reset` 只清理当前会话，不删除人格记忆；终端或 WebUI 的 `/reset all` 会清空当前人格的短期日记、长期日记、知识点、修订记录和待整理状态。主体记忆在一个事务中清理，淘汰上下文随后独立清理。即使后台模型当时正在整理，旧结果也会因数据库身份或记忆代数变化而被拒绝，不能在清理后重新写回；重置前已经启动的其他会话也不能再写入旧日记。

  ![](./pics/记忆.png)

- 深度研究

  >Token 燃烧警告

  重量级插件。对于一个命题，Miyu 可以引经据典，有理有据地进行深度研究并写出研究报告。

  ![](./pics/深度研究.png)

- Linux 输入法问题诊断

  从 Linux 输入法实现原理出发，对软件输入法问题进行深度诊断。

- Fcitx5 wiki 查询

  阅读 Fcitx5 wiki，为输入法问题提供参考。

</details>

## 致谢

#### 功能参考

- [Opencode](https://github.com/anomalyco/opencode) 
- [Claude Code](https://github.com/anthropics/claude-code)
- [Pi](https://github.com/earendil-works/pi)
- [Deepseek-Reasonix](https://github.com/esengine/deepseek-reasonix)
- [Astrbot](https://github.com/AstrBotDevs/AstrBot) 
- [NapCatQQ](https://github.com/NapNeko/NapCatQQ) 


#### 插件设计参考

- [Yue-bin/astrbot_plugin_maskoff](https://github.com/Yue-bin/astrbot_plugin_maskoff)
- [nuomicici/astrbot_plugin_GroupMemberQuery](nuomicici/astrbot_plugin_GroupMemberQuery)
- [advent259141/Astrbot_plugin_Heartflow](advent259141/Astrbot_plugin_Heartflow)
- [Railgun19457/astrbot_plugin_image_generation](Railgun19457/astrbot_plugin_image_generation)
- [xiewoc/astrbot_plugin_weather_wttr_in](xiewoc/astrbot_plugin_weather_wttr_in)
- [muyouzhi6/astrbot_plugin_recall_cancel](muyouzhi6/astrbot_plugin_recall_cancel)
- []()

## 许可

Miyu 使用 MIT License 发布，见 `LICENSE`。
