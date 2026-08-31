# zhihu-search —— 给 AI 用的知乎检索脚本

单文件 CLI，同时是 Miyu script 工具（stdin 传 JSON、stdout 出 Markdown 或 JSON）。
形态对齐 `Miyu/src/scripts/goofish-search`，架构参考 `~/Projects/xhs-search`。

## 用法：只需要传一个 query

**给关键词就搜索，给链接就取全文** —— 不用自己判断内容类型。

```bash
zhihu-search "手冲咖啡"                                    # 搜索
zhihu-search "手冲咖啡" -n 3                               # 3 条, 自动带正文
zhihu-search https://www.zhihu.com/question/629151764     # 取问题下的高赞回答
zhihu-search https://zhuanlan.zhihu.com/p/70527622        # 取专栏文章全文
zhihu-search 19550225                                     # 纯数字 = 问题 id
zhihu-search login                                        # 一次性登录
zhihu-search doctor                                       # 环境自检
```

AI 侧同理，最短就是 `{"query": "..."}`：

```bash
echo '{"query":"手冲咖啡"}'                              | zhihu-search
echo '{"query":"手冲咖啡","limit":5}'                    | zhihu-search
echo '{"query":"https://zhuanlan.zhihu.com/p/70527622"}' | zhihu-search
echo '{"query":"手冲咖啡","format":"json"}'              | zhihu-search
echo '{"action":"doctor"}'                               | zhihu-search
```

可选参数只有 5 个，平时一个都不用给：

| 参数 | 作用 |
|---|---|
| `limit` / `-n` | 搜索条数（默认 10）；query 是问题时表示取几条回答（默认 5） |
| `full` / `--full` `--brief` | 搜索是否带正文。**默认 `limit<=5` 时自动开启** |
| `format` / `--format` | `md`（默认）或 `json` |
| `type` / `--type` | 搜索范围：general / answer / article / people / video |
| `max_chars` / `--max-chars` | 每条正文最大字数（默认单条 8000、多条 3000） |

`action` 也在，但只用于强制（比如把链接当关键词搜）或 `doctor`，正常不需要。

### `--full` 是免费的

搜索接口返回的 `content` 字段**就是完整正文**——已实测：搜索结果里某条 641 字，
单独取同一条回答也是 641 字、逐字一致。所以带正文**不产生额外请求、不增加耗时**，
唯一代价是上下文占用。因此默认规则是「条数少就直接给全文，条数多才退回摘要」。

要拿某条搜索结果的全文，把它的链接直接当 `query` 再调一次即可。

### 出错时

不会只丢一句报错：输出「# 执行失败」+ 错误 + 自动诊断 + **一条**明确的下一步
（`format=json` 时是 `{error, diagnosis, fix}`）。诊断会用异常本身携带的病因——
被风控拦截和登录态失效给出的下一步不同。

## 可行性结论（2026-08 实测）

| 问题 | 实测结果 |
|---|---|
| 官方开放 API？ | 没有。知乎开放平台只有登录授权，没有内容检索。 |
| 匿名能搜吗？ | **完全不能**。web `/api/v4/*`、移动端 `api.zhihu.com`、专栏 `zhuanlan` 全部 `403 code=40352`，跳网易易盾验证页。 |
| 是自动化特征被抓吗？ | **不是**。有头 / 无头浏览器行为完全一致，纯 IP 维度判定。 |
| 签名怎么过？ | `x-zse-96`（配 `x-zse-93: 101_3_3.0`）必需。**没有**小红书 `window._webmsxyw` 那种全局入口——藏在 webpack chunk 里，头名还会在 `93/96` 与 `83/86` 之间切换。 |
| 所以方案是？ | 常驻登录态浏览器，**打开真实页面让站点自己发签名请求**，我们只收 JSON / 读 SSR 的 `js-initialData`。不碰签名算法。 |
| 登录后能跑通吗？ | **能**。search / question / answer / article / doctor 全部正常，单次约 4 秒。 |

和小红书那版的关键差异：xhs 能拿到全局签名函数，可以手搓任意 URI；知乎拿不到，
所以**只能走"页面自己发请求 + 收响应"这一条路**。好处是签名算法怎么换都不用改代码。

## 已知限制（重要）

- **首次要人工过一次网易易盾验证码**，`login` 会开有头窗口让你手动做，之后登录态落在
  持久化 profile（`~/.cache/zhihu-search/profile`，尊重 `XDG_CACHE_HOME`）里自己续期。
  放在缓存目录的代价是：清理工具把它当缓存删掉时，得重跑一次 `login`。
  旧版本装在 `~/.local/share/zhihu-search` 的登录态会在首次运行时自动搬过去，不用重登。
- **账号有风险**。当前用的是主号。自动化访问被判异常时，代价是这个号限频甚至封禁。
  脚本已把节奏压得比 xhs 更保守（串行 + 1.5~3.5s 抖动 + 严格只读）。
- **不能并发**。持久化 profile 上有 Chrome 文件锁，脚本用 `profile.lock` 排队，
  第二个调用会等（默认最多 120s，`ZH_LOCK_WAIT` 可调）而不是撞出难懂的报错。
- **只读**。不做点赞/评论/发布——写操作的封号风险和这个工具的价值不成正比。
- 抓的是公开可见内容，仅供个人调研；别批量采集、商用或再分发。

环境变量：`ZH_HEADED=1` 有头跑（调试），`ZH_VERBOSE=1` json 模式下仍打进度日志，
`ZH_PROFILE_DIR` 换 profile 位置（设了它就不做自动迁移），`ZH_LOCK_WAIT` 调锁等待上限。

## 设计要点

- **一个 `resolve()` 吃掉类型判断**：链接看路径（`/answer/` → 回答，`/p/` → 文章，
  `/question/` → 问题），纯数字按问题，其它当关键词。调用方不需要先分类再选 action。
- **持久化 profile 而不是 storage_state 快照**：知乎的 `z_c0`/`d_c0` 会在会话里自己续期，
  导出再导入的 cookie 更容易被判失效（goofish 那种 25 分钟快照对知乎不够用）。
- **SSR 优先**：单条回答/文章直接读页面里的 `js-initialData`，一次请求拿全文，不用滚动。
  只有搜索和问题页需要收 XHR + 滚动翻页。
- **`z_c0` 作为登录判据**：它是知乎的登录票据，没有它就是游客。
- **风控与登录态分开报**：URL 落到 `/account/unhuman` 或标题含「安全验证」是被风控；
  落到 `/signin` 是登录态失效。两者下一步不同，混成一个提示会让用户做无用功。
- **搜索高亮要清洗**：搜索接口把命中词包成 `<em>`，标题和**作者名**都会被包，
  不清洗会出现「香叔<em>咖啡</em>研究所」这种脏数据。

## 目录

```
zhihu-search       单文件脚本(CLI + Miyu 工具)
index-entry.json   Miyu index.json 的注册项(尚未安装)
```
