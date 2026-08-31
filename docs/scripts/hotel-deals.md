# hotel-deals —— 找某地最便宜的房，顺带看价格趋势

单文件脚本。给定地区和日期，跨平台把房价拉回来排序，告诉你最便宜的是哪几家、
哪个平台订更划算、以及**未来哪天入住最便宜**。

**Google 源零依赖** —— 只用 Python 标准库，`./hotel-deals` 直接跑。
Booking 源需要 playwright，属于可选增强，没装也不影响主功能。

## 可行性结论（2026-08 实测）

| 平台 | 能不能抓 | 怎么抓 |
|---|---|---|
| **Google 酒店** | ✅ 好抓 | 纯 HTTP，**不用浏览器**。价格/评分/评价数都写在卡片的 `aria-label` 上，首屏 HTML 里就有。 |
| **Booking.com** | ✅ 能抓 | 必须真浏览器（纯 HTTP 返回 202 挑战页）。好处是支持 `order=price` 原生按价排序。 |
| Agoda | ❌ 放弃 | 无头浏览器打开是白页，风控挡掉了。 |
| Trip.com / 携程 | ⚠️ 没做 | 页面能开、日期也认，但房源列表在无头下不渲染，得再啃一轮。 |

两个关键发现：

**一、Google 的 `ts` 参数是可以自己拼的。**
`google.com/travel/search` 的日期和币种编码在一个叫 `ts` 的参数里，它是
base64url 过的 protobuf。解出来结构很简单，重新编码能做到**字节级还原**：

```
1: 1
3 { 1 { 2 { 1:<place_id> 7:<place_name> } }      # 有 q= 时可以不填
    2 { 2 { 1{y,m,d}=入住 2{y,m,d}=退房 3=晚数 } 6 { 2:0 } } }
5 { 1 { 7:<currency> } }                          # 币种
```

所以指定任意入住日期只要一个 `urllib` 请求，不用开浏览器、不用点日历。
这既是本脚本能零依赖的原因，也是 `sweep` 能在几十秒内扫完 30 天的原因。

**二、Google 本身就是聚合器。** 它给的"起价"是 Booking / Agoda / 携程 一堆
OTA 里的最低价。所以就算只用 Google 一个源，也已经是跨平台比价了；
再叠一个 Booking 主要是**交叉验证**，以及拿它的全库按价排序
（Google 首屏只有 18 条，是按相关性排的，不是按价）。

## 用

```bash
cd ~/Projects/hotel-deals
./hotel-deals doctor          # 先看看环境
```

```
python      3.14.7
历史库      /home/shorin/.local/share/hotel-deals/history.db
google      ✅ 可用，抓到 18 条（零依赖）
booking     ✅ playwright 就绪（浏览器：chrome）
帮助文本    ✅ 提到的参数都真实存在
```

最后一项是自检：扫一遍所有帮助文本和报错提示里提到的 `--参数`，对照 parser
里真实存在的选项。改了参数名忘了改文档，这里会直接点名并给出行号
（退出码 1，可以挂 CI）。

**查房只要三个参数**：地点、入住日、退房日。

```bash
./hotel-deals -l 东京 -c 2026-10-15 -o 2026-10-17
./hotel-deals -l 东京 -c +14 -o +16          # 日期可以写「几天后」
./hotel-deals -l 东京                        # 全用默认(7 天后住一晚)
```

| 核心参数 | 简写 | 说明 |
|---|---|---|
| `--location <位置>` | `-l` | 地区名。唯一必填项 |
| `--checkin <日期>` | `-c` | 入住日，默认 7 天后 |
| `--checkout <日期>` | `-o` | 退房日，默认入住次日 |

日期三种写法：`2026-10-15`、`10-15`（过了就算明年）、`+7`（7 天后）。

其余的都有默认值，平时不用管（`--help` 里单列在「可选」组）：
`--nights`（不给 `--checkout` 时用它算退房日）、`--source`、`--adults`、`--rooms`、
`--currency`、`--region`、`--top`、`--limit`、`--format`/`--json`、`--no-record`。

参数全具名、位置无关；动作省略时默认 `search`，写在哪都认：

```bash
./hotel-deals -l 大阪 sweep -d 30            # 动作放中间也行
```

`--nights` 默认 1 是有意的：这样 `search` 和 `sweep` 的数字天然可比，
表里每个价都是"每晚多少钱"。

### 查最便宜的房

```bash
./hotel-deals search --location 东京 --checkin 2026-10-15 --checkout 2026-10-17
```

```
Kyoto  2026-09-19 → 2026-09-21 (2 晚, CNY)

#  每晚  2晚总价  评分   评价  来源     酒店                       优惠
─  ────  ───────  ────  ─────  ───────  ─────────────────────────  ────
1  ¥132     ¥263   2.5    416  booking  ez guest house
2  ¥238     ¥477     —      —  booking  Tomona Stay
3  ¥280     ¥561   4.0    469  booking  tsubame-ya
...

同店跨平台价差

酒店                便宜的        贵的          省
──────────────────  ────────────  ───────────  ───
Kyo-no-sato 京の里  booking ¥320  google ¥450  29%
```

没装 playwright 就加 `-s google`，只走零依赖的源。

### 哪天入住最便宜

```bash
./hotel-deals sweep --location 大阪 --checkin 2026-10-10 --days 14
```

```
入住日      周  最低/晚  中位/晚  房源  最便宜的那家
──────────  ──  ───────  ───────  ────  ────────────────────────────
2026-10-10  六     ¥182     ¥278    18  Picnic Hostel Osaka
2026-10-11  日     ¥195     ¥294    18  JA Hotel Midoribashi (綠橋)
2026-10-12  一     ¥135     ¥210    18  The One Five Osaka Sakaisuji
...
2026-10-20  二     ¥109     ¥206    18  Hotel Shin-Imamiya ◀

走势  ▆█▃▃▃▄▄▄▃▃▁▂▄▃  (10-10 → 10-23)
最便宜 10-20（周二） ¥109/晚 · Hotel Shin-Imamiya
最贵   10-11 ¥195/晚  贵 79%
中位 ¥142/晚 · 14 天有数据
```

周末贵、周中便宜的规律一眼就看出来。走 Google（纯 HTTP，6 并发），
30 天也就几十秒。

### 价格是在涨还是在跌

这个得靠攒。每次 `search` / `sweep` 都会自动往 SQLite
（`~/.local/share/hotel-deals/history.db`）落一份快照，跑够几天之后：

```bash
./hotel-deals trend --location 东京
```

```
东京 底价历史  █▅▆▃▁▁
最早 2026-08-24T09:00  ¥171/晚
最新 2026-08-29T09:00  ¥140/晚  ↓ -17.7%
区间 ¥137 ~ ¥171 · 6 个观测点
```

`./hotel-deals places` 看历史库里都攒了哪些地区。

### 让它自己每天跑

```bash
# crontab -e，每天早上 9 点记一次
0 9 * * * cd ~/Projects/hotel-deals && ./hotel-deals -l 东京 -c 2026-10-15 -o 2026-10-17 >/dev/null 2>&1
```

## 两种"趋势"别搞混

| | 横轴 | 回答的问题 | 要等吗 |
|---|---|---|---|
| `sweep` | 入住日期 | 未来哪天住最便宜？ | 不用，跑一次就有 |
| `trend` | 观测时刻 | 同一个日期，房价这几天在涨还是跌？该不该现在订？ | 要，得每天跑攒数据 |

## 输出格式

终端里出对齐表格，管道里自动转 Markdown，`--json` 出结构化数据：

```bash
./hotel-deals search -l 东京 > report.md               # Markdown
./hotel-deals search -l 东京 --json | jq '.offers[0]'  # JSON
```

`--json` 出的是一个**带上下文的对象**，不是光秃秃的数组：

```jsonc
{
  "query":          { "location": "…", "checkin": "…", "nights": 1, "currency": "CNY" },
  "sources_ok":     ["google", "booking"],
  "sources_failed": [{ "source": "booking", "error": "playwright 超时" }],
  "warnings":       ["搜索词含中日韩文字，结果可能混进目标区域之外的酒店…"],
  "returned": 20, "total": 43,
  "offers":   [ /* … */ ]
}
```

`sources_failed` 和 `warnings` 也会出现在 Markdown 输出末尾的「注意」小节里。
**别只看 `offers`** —— 少一个源、或者地名匹配飘了，价格表本身是看不出来的。

## 给程序/AI 调用

argv 为空且 stdin 不是终端时，脚本会把 stdin 上的 JSON 当参数用：

```bash
echo '{"location":"东京","checkin":"2026-10-15","sources":["google"]}' | ./hotel-deals
```

键名就是长参数名（`location`、`checkin`、`no_record`…，下划线连字符都认），
`action` 挑动作（默认 `search`），数组喂给 `sources` 这种可多值的参数，
布尔值喂给 `--no-record` 这类开关。**参数名拼错会直接报错退出**，
不会默默忽略后返回一份「看起来正常但其实是默认参数」的结果。

## 已知的坑

- **非拉丁地名会飘**。Google 那边固定走英文站（酒店名要跟 Booking 的英文原名对得上），
  中日韩文地名过去只能模糊匹配。实测搜「苏州金鸡湖」，返回的最低价那家在**昆山南站**，
  离目标 30 多公里 —— 价格是真的，地方是错的，而且输出不报错、排版也正常。
  所以 `location` 含中日韩文字时会强制带一条 `warning`；要准就用英文地名
  （`Jinji Lake Suzhou`），或者两种都跑一次取交集。

- **`places` 不接受 `--location`**（它就是用来列出所有地区的），
  `sweep` 也不接受 `--checkout`（它扫的是入住日，住几晚由 `--nights` 定）。
  写了会报 `unrecognized arguments` 并点名是哪个参数。
- **Google 首屏只有 18 条**，按相关性排的。所以 `search` 里 Google 的作用是提供
  跨平台起价做交叉验证，真·全库最低价来自 Booking 的 `order=price`。
- **Booking 会限流**。连着请求几次就会开始超时，代码里做了退避重试；
  还是不行就 `--no-headless` 用有头浏览器过一次。
- **评分量表不一样**：Google 是 5 分制，Booking 是 10 分制。代码里记了
  `rating_scale`，显示时统一折算成 5 分制，不然会把 8.1 分的好店
  当成比 4.5 分的店好一倍。
- **跨平台对同一家店的匹配靠名字**，用的是词集合包含率 + 字符相似度
  （`similarity()`）。所以 Google 固定用英文站抓 —— 中文站会把酒店名译成中文，
  和 Booking 的英文原名就对不上了。匹配不上只是少一行比价，不影响主表。
- 抓的都是公开页面，别把并发和频率开太猛。

## 加一个新平台

写个 `xxx_search(place, checkin, checkout, ...) -> list[Offer]`，
登记进 `SOURCES` 就行。要开浏览器的源别拿去给 `sweep` 并发扫。
