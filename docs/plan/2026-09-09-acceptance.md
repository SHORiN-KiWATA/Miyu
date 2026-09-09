# 09-09 十四项优化 · 验收清单

分支 `worktree-opt-2026-09-09`（未提交，等验收）。全量 `cargo test --lib` **1937 passed / 0 failed**。

先准备一个不碰你现有环境的沙箱：

```sh
cd /home/shorin/Documents/github/Miyu/.claude/worktrees/opt-2026-09-09
systemd-run --user --scope -p MemoryMax=12G cargo build     # 约 1 分钟（增量）
```

WebUI 相关的项全部可以在沙箱里看，不用换你的线上二进制。QQ 相关的两项（第一、十三）
需要真机，放在最后。

---

## 一键跑完的自动走查（第 4 / 6 / 7 / 8 / 14 项）

```sh
python3 testkit/webui-links/run.py
```

它会：起一个桩模型（不花额度）→ 起一个隔离 daemon（端口 18411）→ 发一轮对话 →
用 Chromium 逐项断言 + 截图到 `~/.cache/miyu-webui-links/`。

**期望输出结尾是「全过」。** 截图：

| 文件 | 看什么 |
|---|---|
| `01-autolink.png` | 正文里的裸链接全部变成可点的蓝色链接 |
| `02-linkcards.png` | 两张链接卡片：`example.com` 无图小卡、GitHub 带图大卡 |
| `03-tool-pill.png` | 左下角那条超长工具芯片，文字截断在圆角背景**里面** |
| `04-meme-masonry.png` | 表情包瀑布流，瘦高/方/宽三种比例互不牵连 |
| `05-attachment-preview.png` | 点附件芯片弹出的正文预览面板 |

---

## 逐项验收

### 第一项 · 表情包入库失败
**根因**：`src/tools/memes/crud.rs` 是全仓唯一一处把模型返回的 JSON 直接
`serde_json::from_str` 的地方，没有围栏兜底；而提示词自己就把 schema 包在
` ```json ` 里，等于在教模型输出围栏。另外报错吞掉了真实的 serde 信息。

**改了什么**：加 `extract_json_object` 兜底（与 memory organizer 同构）；报错带上
serde 原话和响应开头 200 字；四个结构体去掉 `deny_unknown_fields`（多一个字段不该
让整次入库失败）。

**怎么验**：

```sh
cargo test --lib -- memes::tests
```
应看到 `meme_classification_survives_code_fence_and_preamble` 等 4 项通过。
**真机**：换上二进制后，在 QQ 里引用一张图让她存表情包 —— 之前必失败的那条路现在应
该能入库；如果仍被拒，报错里现在会写清是围栏、少字段还是模型拒答。

### 第二项 · reset-memory 语义
**前提修正**：仓库里原本**没有** `reset-all-memory`，只有 `reset-memory`＝清空整个
人格的记忆。现在拆成两条。

| 命令 | 作用 |
|---|---|
| `/reset-memory`、`miyu reset-memory` | 只清**本次会话**记下的记忆，并告诉你清了几条 |
| `/reset-all-memory`、`miyu reset-all-memory` | 清当前人格的全部长期记忆 |
| `miyu memory reset --session <ID>` | 指定会话 |

QQ、终端、REPL、WebUI 四个入口都有。

**怎么验**（沙箱）：
```sh
H=/tmp/miyu-rm; rm -rf $H; mkdir -p $H
MIYU_HOME=$H target/debug/miyu memory remember "这条没有会话标记"
MIYU_HOME=$H target/debug/miyu reset-memory      # 应说「本次会话还没有记下什么」
MIYU_HOME=$H target/debug/miyu memory stats      # 那条还在
MIYU_HOME=$H target/debug/miyu reset-all-memory  # 这条才清得掉
```
**注意**：这次改动之前存下的旧记忆没有会话标记，`reset-memory` 碰不到它们，只能用
`reset-all-memory`。

### 第三项 · /wipe 不再删技能
**核实结果**：scripts 从来就不删（全链路追过）；skill **确实**会删——`reset_all(true)`
会清掉带 `generated_by: miyu` 标记的自动技能目录。现在改成不删，确认文案同步改了。

**怎么验**：

```sh
cargo test --lib -- reset_command_uses_configured_admins
```
或沙箱里造一个 `SKILL.md`（frontmatter 写 `generated_by: miyu`）放进
`<data>/skills/personas/<人格>/x/`，跑 `miyu wipe --yes`，文件应还在。

### 第四项 · 裸链接可点 ✅ 已自动走查
额外说明：实测抓到一个 bug 并已修——`https://wiki.archlinux.org、AUR` 里的顿号会被
吃进地址（被 punycode 成 `xn--orgaur-kr3e`），现在中日韩标点整类排除在地址字符集外。
行内代码和代码块里的地址一个字节都不动，`<https://…>` 也认。

### 第五项 · 搜索列来源
`web_search` / `web_fetch` 的描述各加一句英文短句，要求用了结果就在文末按
`标题 (URL)` 逐行列出来。

**怎么验**：真机问她一个需要搜索的问题，看回答末尾有没有来源行。
（token 代价：受限平台面 +50 token，见下面的量尺表。）

### 第六项 · 附件预览 ✅ 已自动走查
点附件芯片＝预览（文本类取回正文按等宽排版，PDF 塞 iframe，图片走原来的放大预览），
右边箭头才是下载。后端加了 `?inline=1`，只对 PDF 放行内联；HTML/SVG 永远不内联。

### 第七项 · 芯片截断 ✅ 已自动走查
`.tool-title` 少了 `overflow`、`strong` 又是 `flex: 0 0 auto`。同款写法的
「已思考」标题和「准备工具」标签一并修了。名字被截断时悬浮能看全。

### 第八项 · 链接卡片 ✅ 已自动走查
- 只有**独占一整行**的链接才升级，行内链接不动；每条消息最多 3 张；同一地址只升一次。
- 抓不到元数据 / 超时 / 不是 HTML → 原样保留链接，正文不变形。
- 没有 og:image → 无图小卡（favicon 或域名首字母 + 标题 + 描述 + 域名）。
- 元数据和缩略图**都由 daemon 代抓**，浏览器不直连第三方：WebUI 的 CSP 是
  `img-src 'self'`，放宽等于让模型输出里的任意链接在你浏览器上留一次带 IP 的请求。
- 出站请求过 SSRF 闸门（从 `web_images` 抽成共享的 `src/tools/net_guard.rs`，
  一份实现两处用），DNS 结果逐个校验并钉死，重定向自己走、每跳重校。
- 缓存：成功 6 小时、失败 15 分钟；缩略图按内容哈希落 `cache_dir/link-preview/`。
- 开关：跟随 `plugins.web.enabled`（没有新增配置项，这点你可以否决）。

### 第九项 · goal 三合一
`create_goal` / `get_goal` / `update_goal` → 一个 `goal`，
`action` = get / create / edit / pause / resume / complete / blocked。
续轮提示词里那两条填好的调用串、相关测试、wiki 表格都同步了。

**怎么验**：
```sh
cargo test --lib -- goal
```

### 第十项 · 悬空引用
比 AI 报的更多。全模式错的：`run_command.json` 指 `apply_patch`（真名 `edit`）、两处
`note` 与一条报错指 `job_status`/`job_stop`（真名是 `job(action=…)`）。dev 模式悬空
的：`job`→`read`、`remember_fact`→`kb`、`load_tools` stub 提示→`manage_script`。

**根因是「描述层按母工具集写、暴露层按模式裁」**，所以修法不是改字面量了事：新增
`src/tools/cross_hints.rs`，这类跨工具指路句从常量描述里摘出来，注册全部结束后按
**实际注册表**补——被指的工具没注册就一个字都不加。

**怎么验**：

```sh
cargo test --lib -- cross_hints
```
`hint_lands_only_when_the_named_tool_is_registered` 就是钉这件事的。

### 第十一项 · task/job 的 token
**前提部分不成立**：前台 `task` 早就带了完整用量（返回里的 `stats:` 一行有
prompt/completion/total/cache_read 和估算标记）。真正没有的是 **`job`**。

**改了什么**：`job(action=status)` 现在对子代理任务多回一个 `tokens` 字段；命令类任务
不走模型，这个字段整个不出现。顺带修掉一个既有 bug——`task.rs` 后台路径还在按旧 JSON
形态解析 state，而 08-21 已改成纯文本，于是永远 fallback 成 `"completed"`，
`budget_reached` 被误报为正常完成。

**怎么验**：
```sh
cargo test --lib -- usage_tests
```

### 第十二项 · 知识库
**两半是同一个 bug。** 代理从生产日志里翻到了原始报告：报告人在 macOS 上用自定义人格，
「加载知识库」指的是往库里传文档，撞的就是那道词表闸门。「知识库与人格无关」这点用真
注册表在默认/自定义两个人格下各建一次工具面实测钉住了（48 件工具、`kb` 写入、
`search_knowledge_base`、`read kb:` 全一致）。

**改了什么**：`reject_non_kb_upload` 原先拿 `skill`/`memory`/`config`/`记忆`/`配置`
去扫**整篇正文**——一篇讲内存管理的文档、任何出现过 config 的教程都进不来。现在只认两
样不会误伤的证据：落点路径是不是 Miyu 自己的资产目录（`SKILL.md`/`skills/`/`personas/`
/`config.toml`/`memory.db`…），以及正文是不是一份带 `generated_by: miyu` 标记的技能文件。

顺带修了一个 macOS 隐患：`safe_file_path` 在库目录还不存在时先取真实路径、失败后退回
未解析路径，再和已解析的父目录比——路径里有一层符号链接就对不上（macOS 的
`/tmp`→`/private/tmp` 是现成的踩法），第一次往新库写文件必报「逃出目录」。现在先建目录
再取真实路径。

**怎么验**：

```sh
cargo test --lib -- knowledge_base
```
或在 WebUI 知识库面板手动上传一篇正文里带「配置」二字的 md，应该能传上去。

### 第十三项 · 赞助记账（新功能）
**模型侧**：`sponsor` 一个工具，只在通讯平台会话注册，懒加载（full 模式照样全量给出）。
`action` = add / list / query / update / delete。金额按分存整数，不用浮点。
美元**在记账当刻**按实时汇率折成人民币并把汇率冻结在行里——排行榜要的是稳定名次，不能
每次打开都重拉汇率让历史名次自己晃。拉不到汇率也照记，只是这笔暂不计入人民币榜
（宁可榜上少一笔，也不往账本里塞一个编造的数）。
写限管理员，**软拒绝**（返回 `ok:false` 加一句说明，不是 Err）；看开放。不做二次确认。

**面板**：控制台新增「赞助」，和其它面板同一套设计语言。总额/人数/笔数/最近一笔四块
统计卡 → 前三名领奖台 + 名次条形榜（可切按金额/按笔数/按最近）→ 明细表（改备注、删）。
未折算的记录挂「未折算」芯片，绝不显示成 ¥0。

**怎么验**：

```sh
cargo test --lib -- sponsor        # 22 项
```
面板截图在 `~/.cache/miyu-sponsor-shots/`（13 张，含桌面/手机/空库/单条/各种抽屉）。
**真机**：在 QQ 里以管理员身份说「记一笔赞助，10001 给了 30 块，备注买咖啡」，然后
「赞助榜」；再用非管理员账号试记一笔，应该收到一句说明而不是报错。

### 第十四项 · 表情包瀑布流 ✅ 已自动走查
改成「1px 行高 + 按实际高度算跨行数」的瀑布流，列还是自动填充所以**从左到右的顺序不
变**（搜索命中的前几张仍在最前，CSS 多列布局做不到这点）。极端比例夹在 0.55–1.9 之间，
免得 1:5 的长条把一整列拉成走廊。

实测抓到一个真 bug 并已修：`aspect-ratio` 会被 flex 子项的**自动最小高度**顶掉——一张
120×420 的图照样撑出 646px 的格子。加 `min-height: 0` 才生效。

---

## 量尺：工具面 token（改前 → 改后）

| 面 | 改前 | 改后 | 差 |
|---|---:|---:|---:|
| normal/stub | 3226 | 3198 | **−28** |
| normal/full | 7140 | 7137 | −3 |
| dev/stub | 1596 | 1575 | **−21** |
| dev/full | 2498 | 2483 | −15 |
| restricted/stub | 1159 | 1209 | **+50** |
| restricted/full | 2196 | 2246 | +50 |

goal 三合一省下的比「列来源」那两句花掉的多，所以 normal / dev 两面净减；受限平台面没有
goal，只吃到 +50。

## 门禁

- `cargo test --lib` 1937 passed / 0 failed
- `python3 test_scripts/fmt_no_regress.py` 通过
- `bash test_scripts/check-model-english.sh` 通过（模型可见文本无中文、JSON 合法）
- `python3 test_scripts/arch_dep_check.py` 未通过，但**与 main 逐字相同**（存量违规，本次
  一条新的跨层引用都没加，已在 main 的临时工作树上对拍确认）
- `refactor_size_report.py`：越红线文件 0 个，最大文件 1374 行

## 还没做 / 需要你拍板

1. **缓存契约的两轮手测**（AGENTS §1.6）。改了工具描述＝一次计划内冷启动；但「第二轮
   `cache_read` 不异常下降」这条得用真供应商测，桩模型不报缓存。建议换上二进制后随便
   聊两轮，看 `~/.miyu/cache/logs/cache-usage.*.jsonl` 第二条的 `cache_read`。
2. **链接卡片的开关**目前跟随 `plugins.web.enabled`，没有独立配置项。要独立开关我再加。
3. **`fx_source: "manual"`**：面板手填 `cny_minor` 时记这个值，不在工具侧文档的取值集里。
   要不要统一成别的值由你定。
4. **`next-release-note.md`** 等你验收通过后我再写（按约定，且那个文件在主检出里）。

---

# 第二轮（09-09 验收反馈后）

用户验收结论：第 1/2/3/4/5/9/12/13 项通过；第 6/7/8 项部分通过；第 11 项要求回滚；
新增「知识库面板支持拖拽与批量上传」。

## 第 11 项 · 回滚

`job` 的 `tokens` 字段、`JobTokenUsage`、`record_job_usage`、`job_usage()` 及其测试
全部撤掉。

**保留**了同一批改动里那个独立的真 bug 修复：`task.rs` 后台路径原先按旧 JSON 形态
解析 state，08-21 改纯文本后永远 fallback 成 `"completed"`，`budget_reached` 被误报为
正常完成。现在 `run_task_core` 直接把 state 带出来（`TaskRun`）。这跟「给模型看代价」
是两回事，不在回滚范围。

## 第 6 项 · 视频附件

`kindOf()` 原先把 `video/*` 归成 `binary`，芯片就退化成一个下载链接。现在多了
video / audio 两档，弹出的是带控件的播放器；后端本来就对图片/音视频内联交付真 MIME，
不需要 `?inline=1`。关面板时会 `pause()`，免得在背后继续出声；浏览器不认这个编码时
给一句「下载下来用本地播放器打开」，不留黑框。

**实测**：走查里真的用 ffmpeg 生成一段 1 秒 mp4 上传，断言弹出的是 `<video>`、
`videoWidth/Height` 解出 160×120、`readyState=4`。截图 `06-video-preview.png`。

## 第 7 项 · 「已思考」被压成「已」

**这条我没能复现。** 无头环境里无论 `min-width: 0` 还是 `4em`，真实那枚芯片都不截断
（可见 38px / 内容 34px）。所以不去猜是哪一层容器按 min-content 定宽，改成一个不依赖
机制的写法：

- `.reasoning-title` 改回 `flex: 0 0 auto`——**根本不参与压缩**，短标题在任何容器宽度
  算法下都不会被牺牲。长度由 `max-width: 24em` 封顶（模型自带标题最长 160 字符）。
- `.reasoning-block summary` 加 `overflow: hidden`，极端长度裁在圆角上，不会画出背景。

工具芯片那半（长 tag 省略）用户已验收通过，保持可压缩，但补了 `min-width: 4em` 下限。

**要你复看的就是这一条**：换上二进制后看「已思考」是否恢复完整。

## 第 8 项 · YouTube / Bilibili 不出卡

两个不同的根因，都已修：

| 站点 | 根因 | 修法 |
|---|---|---|
| YouTube | og 标签在第 **705 883** 字节（`<head>` 里塞满内联 JS），而我死板地只读前 256 KB | 改成读到 `</head>` / `<body` 为止，硬顶放到 2 MB。跨 chunk 的标记按 8 字节重叠扫 |
| Bilibili | 其实抓得通（实测 `ok:true` 0.6s）。用户那次多半是抖了一下，然后被**负缓存钉死 15 分钟** | 负缓存分档：「就是做不出卡」（非 HTML／无标题／地址不合规）仍记 15 分钟；网络抖动／超时只记 45 秒 |

另外两处：

- 抓页面与抓图片的预算分开（12s / 8s），图片失败不再拖垮整张卡。
- favicon 收 ICO 了（半数站点的 `rel=icon` 还是 .ico，不收就只能画首字母）。**不收 SVG**
  ——它能带脚本，而这些字节最后是从本机同源发出去的。
- 卡片名额改成按「**真的做出卡片**」计数，最多试 6 条、成 3 张。用户那次第一条
  bilibili 抓挂了却白占一个名额，排在后面的维基链接连试都没试。

**实测**（真联网，非桩）：YouTube 1.1s 出卡带图、Bilibili 0.6s、维基 1.8s 带图、
GitHub 0.8s 带图带 favicon。走查里断言「3 张卡且其中有 YouTube 且带图」「第四条保持
纯链接」。

## 新增 · 知识库面板拖拽 / 批量上传

见对应提交。

## 第二轮门禁

- `cargo test --lib` 全过
- `python3 testkit/webui-links/run.py` 全过（比第一轮多了视频附件、YouTube 卡片、
  名额策略、真实「已思考」芯片四组断言）
