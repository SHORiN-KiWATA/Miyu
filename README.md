<p align="center">
  <img src="pics/miyu-logo.png" alt="Miyu" width="180">
</p>

# Miyu

一个活在终端里的二次元少女。

>暂时

## 谁是 Miyu？

Miyu 是从我曾经很喜欢的动画中的角色[久遠寺未有](http://www.minatosoft.com/kimiaru/chara-miyu.html)身上汲取灵感制作的人物。

![](./pics/miyuwallpaper.png)

## 有什么功能？

`miyu` 由大模型驱动，默认接入了 [opencode](https://github.com/anomalyco/opencode) 的公共模型服务，你也可以配置自己的大模型服务。她并非专业的 Coding Agent，而是更偏向聊天日常、游戏娱乐、系统排障等日用场景。并且 `miyu` 还可以无缝与 `fish`、`zsh`、`bash` 集成，终端打字直接无缝对话！

![](./pics/shell-init.png)

`miyu` 还自带了 TUI 方便修改配置。

```
miyu config
```

![](./pics/tui.png)

## 如何安装？

- Arch Linux

  ```
  yay -S miyu
  ```

### 内置插件

<details><summary>[展开/收起] 具体介绍</summary>
<br>

- 表情包
  
  表情包毫无疑问是聊天时最重要的部分，在对话时，Miyu 会根据情景自主发送符合情境的表情包。除了自主发送，设置里还可以设置概率、置信度和冷却时间。

  ![](./pics/nvidiafuckyou.png)

  Miyu 自带了一些表情，存放在`/usr/share/miyu`，对应的用户空间目录是`~/.local/share/miyu`。表情库是跟随人格的，如果你在设置里新建了自己的人格，那么就无法使用 Miyu 的默认表情。你可以准备一些图片，把路径给 Ai，让其保存到表情库。届时会自动调用识图模型对图片进行分析并保存。Miyu 默认使用 opencode 公共模型服务中的多模态模型进行识图，所有即使不配置自己的视图模型也可以看图片。

- 玄学算命

  >心理学。
  
  算命就像看天气预报一般稀松平常。Miyu 自带了周易六十四卦、吉凶占、塔罗牌抽取等玄学功能。

  ![](./pics/玄学.png)

- 闹钟

  >要我说，这比GNOME时钟的闹钟好用多了
  
  Miyu 自带了闹钟，日常泡泡面、番茄钟学习、计时任务什么的都很实用。内置了闹钟音频，你还可以通过路径传入你想要在到点后播放的“闹钟”。

  ![](./pics/set_alarm.png)

- 知识库

  Miyu 自带了 [ShorinWiki](https://github.com/SHORiN-KiWATA/Shorin-ArchLinux-Guide) 中的内容和一些日用 Linux 会遇到的问题作为默认知识库。

  当然，你也可以通过 `miyu kb` 命令，或者通过跟 AI 的自然语言交互管理属于你自己的知识库。

  ![](./pics/kb.png)

- Linux 游戏兼容性调查

  >这个游戏 Linux 能玩吗？

  这是桌面端使用 Linux 的日经问题，Miyu 会去 [ProtonDB](https://www.protondb.com/)、[Are We Anti-Cheat Yet?](https://areweanticheatyet.com/)、[Can I Play On Linux](https://caniplayonlinux.com/)等 Linux游戏兼容性资讯网站获取主要信息，辅以社区玩家的声音，综合判断一款游戏的兼容性并提出建议和注意事项。

  ![](./pics/gaming.png)

- 网络搜索

  即使不配置网络搜索 API，Miyu 也仍然拥有基础的网络搜索和网页读取能力。可以在插件配置中设置 Tavily、Firecrawl 、AnySearch、SearXNG 等网络搜索 API 以获得更佳的搜索效果。

  ![](./pics/web-search-config.png)

- 搜图

  Miyu 还能帮你找图片喔！图片会默认保存至`XDG图片目录/Miyu`。

  >NSFW 禁止！

  ![](./pics/搜图.png)

- 生图

  支持 OpenAI 的画图服务喔。图片会默认保存至`XDG图片目录/Miyu`。

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

  Miyu 的记忆由两部分组成，其一是“曾经发生的事”，其二是“信息中的知识点”。对话时会根据用户消息自动召回条目，这是联想功能。

  ![](./pics/记忆.png)

- 深度研究

  >Token 燃烧警告

  重量级插件。对于一个命题，Miyu 可以引经据典，有理有据地进行深度研究并写出研究报告。

  ![](./pics/深度研究.png)

</details>

## 做出贡献

<details><summary>[展开/收起] 如果你想要一同开发 Miyu 请先阅读下面的内容</summary>
<br>

### 设计理念

Miyu 的定位是桌面助手，不是 Coding Agent，她不进行任何复杂的项目开发工作，更注拟真、系统集成度、实用、日常排障等方面。Miyu 应该开箱即用，并且足够轻量，不开发超重的 3D 桌宠，不使用 GUI 框架，也不设计需要学习成本的CLI 选项，尽量通过自然语言和无缝无感的触发方式进行所有的操作。

以下是一些可能的方向：

- 提升系统日常排障能力、系统维护能力

  作为桌面助手，尤其是 Linux 桌面端助手，对日常问题的排障能力是重重之中。她应当能够解决日用系统会遇到的问题，如输入法异常、显卡驱动异常、桌面软件崩溃等。

- 知识和信息

  增加对软件推荐、游戏兼容性调查、时事新闻、学习辅助等非开发场景下会出现的情景的处理能力。增加知识和信息检索的时效性和可靠性也是关键点。

- 提升角色扮演能力，提高对话娱乐性和拟真度

  需要更多像“发送表情包”、“玄学算命”那样提升对话时的趣味性或拟真度的功能。TTS、语音对话等重要功能也在日程上。

- 提高和系统的无缝集成

  不使用任何命令作为触发器。能够直接使用自然语言开启对话。目前是通过 Command Not Found 内容交给 Miyu 的方式做到和终端的无缝集成，但是终端逐行解释命令的特点导致无法在终端进行多行的无缝对话，如何进行多行的无缝对话是一个需要研究的点。
  
  终端以外的集成也值得研究，例如做成守护进程以拥有持续运行的能力，监听系统事件，在特定事件发生时做出特定反应等。

- 优化功能和修复 BUG

  在不变更设计语义，不影响现有功能实现的效果的前提优化运行表现，修复 BUG。

### 如何 PR

一个 PR 仅包含一个功能，若包含多个功能，应当拆分后提交多个 PR。应当提交


</details>

## 许可

Miyu 使用 MIT License 发布，见 `LICENSE`。
