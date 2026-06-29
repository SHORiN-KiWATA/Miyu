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

## 如何安装？

- Arch Linux

  ```
  yay -S miyu
  ```

### 内置插件

`miyu` 有一系列默认插件和功能，自带了 TUI 方便修改配置。

```
miyu config
```
![](./pics/tui.png)

- 表情包
  
  表情包毫无疑问是聊天时最重要的部分，在对话时，Miyu 会自主发送符合情境的表情包。

  ![](./pics/nvidiafuckyou.png)

  Miyu 自带了一些表情，存放在`/usr/share/miyu`，对应的用户空间目录是`~/.local/share/miyu`。表情库是跟随人格的，如果你在设置里新建了自己的人格，那么就无法使用 Miyu 的默认表情。你可以准备一些图片，把路径给 Ai，让其保存到表情库。届时会自动调用识图模型对图片进行分析并保存。

- 玄学算命

  >最适合中国宝宝的心理学。

  ![](./pics/玄学.png)

- 闹钟

  >泡个泡面，记个时。
  
  Miyu 自带了闹钟音频，你还可以通过路径传入你想要在到点后播放的“闹钟”。

  ![](./pics/set_alarm.png)

- 知识库

  Miyu 自带了 [ShorinWiki](https://github.com/SHORiN-KiWATA/Shorin-ArchLinux-Guide) 中的内容和一些日用 Linux 会遇到的问题作为默认知识库。

  当然，你也可以通过 `miyu kb` 命令，或者通过跟 AI 的自然语言交互管理属于你自己的知识库。

  ![](./pics/kb.png)

- Linux 游戏兼容性调查

  >这个游戏 Linux 能玩吗？

  ![](./pics/gaming.png)

- 网络搜索

  即使不配置网络搜索 API，Miyu 也仍然拥有基础的网络搜索和网页读取能力。可以在插件配置中设置网络搜索 API 以获得更佳的搜索效果。

  ![](./pics/web-search-config.png)

- 搜图

  miyu 还能帮你找图片喔！

  >NSFW 禁止！

  ![](./pics/搜图.png)

- 生图

  支持 OpenAI 的画图服务喔。

  >这个功能默认用不了，要自己在插件设置里开启并配置 API

  ![](./pics/生图.png)

- 天气查询

  查询天气是每天的必做活动，当然少不了。

  ![](./pics/weather.png)

- 汇率查询

  国际社会，查个汇率也很合理吧？

  ![](./pics/汇率.png)

- Man 手册查询

  >Man！如果 AI 运行命令前能查询 Man 就好了
  
  ![](./pics/man.png)

- Arch Linux相关

  Arch Linux 是桌面 Linux 的热门之选，Miyu 有一系列插件可以帮助提高 Arch Linux 的日用体验。

  - AUR 状态查询

    >AUR还在被 DDos 吗！
    
    ![](./pics/aur-status.png)

  - Arch Wiki 查询

    作为 “Linux 圣经”，查询 Arch Wiki 不仅能提高日用 Arch 的体验，对其他发行版也大有裨益。

    ![](./pics/archwiki.png)

  - PKGBUILD 审查

    AUR 投毒的事件搞的人心惶惶，但现在，Miyu 可以帮我审查 PKGBUILD 啦！

    ![](./pics/pkgbuild审核.png)

- 文件操作

  >自不必说。

  Miyu 支持读写文件、搜索内容、查找文件、删除文件等。

- 计算器和哈希编解码

  为了计算结果的准确性，Miyu 自带了科学计算器和哈希编解码的能力。

  ![](./pics/hash.png)

- 记忆系统

  Miyu 的记忆由两部分组成，其一是“曾经发生的事”，其二是“信息中的知识点”。

  ![](./pics/记忆.png)

- 深度研究

  >Token 燃烧警告

  重量级插件。对于一个命题，Miyu 可以引经据典，有理有据地进行深度研究并写出研究报告。

  ![](./pics/深度研究.png)

## 许可

Miyu 使用 MIT License 发布，见 `LICENSE`。
