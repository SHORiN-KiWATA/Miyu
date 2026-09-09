# reddit-search —— 给 AI 用的 Reddit 检索工具

一个单文件脚本，形态对齐 `src/scripts/personas/default/zhihu-search`：既能直接当命令行用，
也能作为 Miyu script 工具给 AI 调（stdin JSON → stdout md/JSON）。**零依赖**，只要系统
`python3`。数据源只有一个：**Arctic Shift 归档**。

## 为什么不用 Reddit 官方 API（2026-09-09 实测）

| 探测 | 结果 |
|---|---|
| 匿名 `www.reddit.com/search.json` | **403**，回的是网页外壳不是 JSON |
| `old.reddit.com/search.json` | 302，跟到同一堵墙 |
| `oauth.reddit.com` 无 token | **403** |
| `POST /api/v1/access_token` 假凭据 | 401 `{"message":"Unauthorized"}`（端点通，缺的是凭据） |
| 拿凭据的门槛 | Reddit 已改政策，**API 凭据要审核**，不再是谁都能申请 |
| `api.pullpush.io`（另一个 Pushshift 后继） | **429**，响应体直说不再提供免费抓取资源 |

一开始写了「官方 OAuth 优先 + 归档兜底」的双层版本，凭据关过不去就等于那半边永远空转，
**已整条移除**：没有凭据存储、没有 token 缓存、没有额度冷却、没有 `backend` 参数。
少了一半分支，也少了一半会坏的地方。

## Arctic Shift 能做什么、不能做什么（实测）

| 问题 | 实测结果 |
|---|---|
| 数据新不新 | 很新，抓到过 **1 分钟前**发的帖 |
| 全站关键词搜索 | **不能**。不带范围时报 `'query' query parameter requires one of: author, subreddit` |
| 按热度排序 | **不能**，只有 `created_utc` 的 asc/desc |
| 赞数准不准 | 发帖后约 **36 小时**才回填，之前 `score`/`num_comments` 一律是 1/0 |
| 关键词检索快不快 | **慢且飘**。冷启动第一次几乎必回 `Timeout. Maybe slow down a bit`，热起来之后连续 5 次全过。整条查询实测 3–16s |
| 关键词 + author 不带时间窗 | **基本必超时**（退避重试 4 次、31s 仍失败）；同一条查询补上 `after=2026-01-01` 后立刻通 |
| 评论树结构 | 与 Reddit 官方**完全同构**（`{"kind":"t1","data":{…,"replies":{"kind":"Listing",…}}}`） |
| `fields` 参数 | 没有 `permalink` 这个字段（传了直接报错），链接得自己按 `/r/{sub}/comments/{id}` 拼 |
| 正文有时是 `[removed]` | 归档在帖子刚发出时抓，而 r/OpenAI、r/ClaudeAI 这类大版块的 AutoModerator 是**秒级**过滤的，抓到时正文已经被撤了。评论区照样完整——所以这种帖子仍然有读的价值，别当成脚本抓漏了 |

## 动作面

| action | 说明 |
|---|---|
| `search`（默认） | 有 `query` = 关键词检索（**必须同时给 `subreddit` 或 `author`**）；无 `query` = 版块/作者的帖子列表 |
| `comments` | 读一个帖子 + 它的评论树。`query` 里放 reddit 链接或裸 id 会**自动**切到这里 |
| `user` | 某人的帖子和评论（`kind` 选 posts/comments/both） |
| `subreddits` | 按名字找版块（先前缀匹配，没命中再全名匹配） |
| `doctor` | 自检：分别探「普通检索」和「关键词检索」两条索引，各自报耗时 |

## 做不到的事，脚本是怎么处理的

| 做不到 | 处理 |
|---|---|
| 全站关键词搜索 | **不硬凑**。直接 `{"ok":false,error,fix}` 退出，**46ms，一个请求都不发**；fix 教它补 subreddit/author，或先用 `action=subreddits` 找版块 |
| 按热度排序 | `sort=top` 走窗口采样，见下 |
| 36 小时内的赞数 | 显示成「赞数未结算(归档 36h 后回填)」，**不显示成 1↑** —— 否则模型会照着说「这帖没人理」 |
| 关键词检索冷启动超时 | 退避重试 1s / 3s / 6s（共 4 次）。放弃时 fix 里写明重试过几次，并给出真正管用的下一步（补时间窗） |

### sort=top 的窗口采样

归档单次最多回 100 条且只能按时间排。直接抓 100 条排序 = 「最近 100 条里的最高分」，
对 r/LocalLLaMA 这种一天上百帖的版块，实测 `time=month` 抓回的 100 条**全在 24 小时内**，
等于把「最新」冒充成「最热」。

现在的做法：把窗口切 5 段分别取样、合并去重后按分数排，并且**排除最近 36 小时**（那段
赞数还没回填）。未结算的一律排到已结算的后面——分数未知不等于分数低。

实测 `{"subreddit":"LocalLLaMA","sort":"top","time":"month"}` 前后对比：

| | 第 1 名 | 第 2 名 | 第 3 名 |
|---|---|---|---|
| 直接抓 100 条 | 未结算 · 20 小时前 | 未结算 · 5 分钟前 | 未结算 · 19 分钟前 |
| 5 段采样 | **2513↑** · 13 天前 | **1351↑** · 7 天前 | **1290↑** · 13 天前 |

代价：5 次请求、10–16s（普通列表 1.4–2.3s）。结果的 notes 里会写明「分 5 段取样共 500 条」
以及「有几段撞到 100 条上限，所以这是样本不是完整排名」——不把采样说成排名。

## 真实 API 实测（2026-09-09，退避重试落地后）

| 用例 | 结果 | 耗时 |
|---|---|---|
| 版块列表 | ✅ | 2.3s |
| 版块内关键词 | ✅ | 15.1s（含重试） |
| `sort=top` 一个月 | ✅ | 16.0s |
| `sort=top` 一周 | ✅ | 10.6s |
| 全站搜索（应快速失败） | ✅ 按预期报错 | **0.05s** |
| 读帖 + 评论树 | ✅ | 3.2s |
| 用户帖子+评论 | ✅ | 3.3s |
| 版块搜索 | ✅ | 1.8s |
| doctor | ✅ 两项全绿 | 12.9s |
| 时间区间 after/before | ✅ | 1.4s |
| author + 关键词、无时间窗 | ❌ 超时（归档侧限制，fix 已指明补时间窗） | 31.9s |

### 一次真实使用（搜「astra vs fable 5.1」）

拿它当真活干了一趟，工作流是「先 `subreddits` 定版块 → 再在版块里搜 → 挑帖子读评论」。
顺带把两条限制的手感摸清了：

| 观察 | 数据 |
|---|---|
| 找版块 | `subreddits query=claude` 1.9s，r/ClaudeAI 排第一（146.3k 订阅） |
| 同一版块、同样写法，成败取决于命中量 | `query=astra` + `after=30d` 在 r/ClaudeAI **6.7s 出结果**；`query=fable` 同参数**退避 4 次全超时**（fable 在这个版块几乎每帖都提），把窗口收到 `after=2d` 后 **一次就过** |
| 所以 fix 里那句「加 after/before」是实测出来的，不是套话 | r/OpenAI 搜 fable 同样：不带窗超时 18.5s，`after=3d` 立刻出 8 条 |
| 评论树 | 读 2D sprites 对比帖，12 条评论含三层嵌套，3.4s |

## 离线测试

```bash
python3 testkit/reddit-search/run.py     # 64 项，全离线
```

`testkit/reddit-search/` 起一个假 Arctic Shift 顶替 `ARCTIC_BASE`，再把脚本复制一份、
**只改这一个常量**用子进程跑——走的是真的 stdin JSON 契约和真的 HTTP 栈，只有对面是假的。
真服务器不会听你的：限流、查询超时、返回畸形、字段缺失恰恰是最需要测的分支。

覆盖：r//u/ 前缀剥离、缺 permalink 时自拼链接、36 小时结算边界、未结算不被写成分数、
全站搜索零请求快速失败、top 采样 5 段与未结算排序、超时退避重试上限、429/500/非 JSON/
连不上四类故障各自的报错与 fix、评论树层级与折叠节点、depth/limit 限制、
链接与 `t3_` 前缀识别、版块搜索的前缀→全名两跳、doctor 红绿、参数边界夹取、命令行形态。

## 命令行

```bash
reddit-search -r LocalLLaMA -n 5                    # 版块最新 5 条
reddit-search -r LocalLLaMA -s top -t week          # 本周高分（窗口采样）
reddit-search quantization -r LocalLLaMA            # 版块内关键词
reddit-search https://reddit.com/r/x/comments/abc/  # 读帖+评论
reddit-search -a subreddits localll                 # 找版块
reddit-search -a doctor                             # 自检
```
