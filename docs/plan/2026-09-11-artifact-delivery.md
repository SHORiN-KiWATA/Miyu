# WebUI 成果交付能力增强 · 调研与方案（2026-09-11）

todolist「Feats / 增强webui成果交付能力」的调研。结论先行：

> **右侧分栏、HTML 渲染这些 Miyu 早就有了。差的不是面板，是面板里的东西是活的还是死的。**
> 现在 HTML artifact 里的 JavaScript 一行都不跑——被前后端两道锁焊死。放开这两道锁之后，
> 交互能活，而六条数据外带的路仍然全封。这一条已经在本机预演实测过，见 §2。

调研方式：源码通读 + 真机探针实测（`testkit/webui-artifact/`，沙箱 daemon + 桩模型 + Playwright）。
所有"现状"数字都是跑出来的，不是读代码推的。

---

## 一、现状基线（实测）

一轮里让她用 `artifact` 工具写四份探针文件，逐个在右侧面板里看。二进制 `miyu 0.5.0`（main @ e78568fb）。

### 1.1 面板已经有的

| 能力 | 现状 |
|---|---|
| 右侧分栏工作区 | ✅ 可拖宽、全屏、多资源下拉菜单、单份移除 |
| Markdown 预览 | ✅ 纯 DOM 渲染，KaTeX 公式、表格、代码块高亮都在 |
| HTML 预览 | ✅ 有 iframe（但见 1.2） |
| PDF 预览 | ✅ iframe 内联 |
| 图片预览 | ✅ 缩放 / 平移 / 新标签打开 |
| 源码视图 | ✅ 带行号 |
| 复制 / 下载 | ✅ `?download=1` 强制 attachment |

### 1.2 HTML artifact 的六格探针

`probe.html` 里放六格自报家门的探针，看哪格活了：

| 探针 | 实测结果 | 拦它的是谁 |
|---|---|---|
| A 内联 CSS | ✅ 生效 | — |
| B 内联 `<script>` | ❌ **NOT RUN** | iframe `sandbox=""` |
| C 外部 CDN 脚本 | ❌ NOT LOADED | CSP `default-src 'none'` |
| D 外链图片 | ❌ naturalWidth=0 | CSP `img-src data: blob:` |
| E `data:` 图片 | ✅ naturalWidth=60 | — |
| F 按钮 `onclick` | ❌ 死的 | 同 B |

两道锁的原文：

- 前端 `web/app.js:5301` — `frame.setAttribute("sandbox", "")`（空值 = 全禁，脚本也禁）
- 后端 `src/web/assets.rs:626` — `Content-Security-Policy: sandbox; default-src 'none'; style-src 'unsafe-inline'; img-src data: blob:`

浏览器控制台的原话：`Blocked script execution in '...' because the document's frame is sandboxed and the 'allow-scripts' permission is not set.`

**能画的只有内联 CSS + data: 图。** 图表、mermaid、tab 切换、排序筛选、任何 React/Vue 应用——全是死的。

### 1.3 其它格式的缺口

| 格式 | 实测 | 说明 |
|---|---|---|
| `.svg` | ❌ 预览、源码两个按钮**都是 hidden** | `artifact_media_type` 没认 svg → `octet-stream`/`file`，两个 supports 判定都不过。面板里只剩一坨不换行、不高亮的字节 |
| `.csv` / `.tsv` | ⚠️ 只有源码视图，`tables=0` | 归为 `text`，没有表格视图 |
| markdown 里的 ` ```mermaid ` | ⚠️ `mermaidRendered=0` | 当普通代码块渲染（这是刻意的，见 §3.3） |
| markdown 里的 `$...$` | ✅ `katex=2` | KaTeX 已 vendored |
| **源码视图的语法高亮** | ❌ `tokenSpans=0` | `renderArtifactSource` 只做 `code.textContent = text`，没接 `MiyuHighlight`。聊天正文里的代码块是有高亮的，面板里没有 |

### 1.4 面板之外的交付链路

| 项 | 现状 |
|---|---|
| 用户在面板里改 artifact | ❌ 只有 `GET /api/artifacts/{id}`，没有写路由 |
| 版本历史 / 回滚 | ❌ `asset_id = hash(session_id + 路径)`，同名 upsert 直接盖掉旧版 |
| 新标签页打开 | ⚠️ 只有图片有这个按钮，HTML/PDF/MD 没有 |
| 对外分享链接 | ⚠️ `share_file` 是另一套（走磁盘、`require_auth`）；artifact 没有分享入口 |
| 导出（md → PDF/HTML） | ❌ 只能下原文件 |
| QQ 端交付 | ❌ QQ 侧完全没有 artifact 概念（`grep` 零命中） |
| 系统提示词里提过 artifact 吗 | ❌ `src/prompts/*.md` 零命中。她只能从工具描述里知道有这回事 |

---

## 二、核心方案：把 HTML 放活，同时封死外带（已预演实测）

### 2.1 改什么

两处，各一行量级：

```
web/app.js:5301        sandbox=""  →  sandbox="allow-scripts allow-modals"
src/web/assets.rs:626  CSP 换成下面这条（{origin} 从请求的 Host 头拼，局域网访问才不会瞎）
```

```
sandbox allow-scripts allow-modals;
default-src 'none';
script-src 'unsafe-inline' 'unsafe-eval' {origin};
style-src  'unsafe-inline' {origin};
font-src   data: {origin};
img-src    data: blob: {origin};
media-src  data: blob: {origin};
connect-src 'none';
form-action 'none';
frame-src 'none';
base-uri 'none'
```

关键点，一个都不能少：

1. **不给 `allow-same-origin`。** iframe 拿到不透明源（opaque origin），读不到父页面的 DOM、cookie、localStorage。这是隔离的地基。
2. **不给 `allow-popups` / `allow-forms` / `allow-top-navigation`。** 这三个各自是一条外带通道（`window.open`、表单 GET、`top.location`），CSP 管不了，只有 sandbox 管得了。
3. **`connect-src 'none'`。** 连本机 API 都不许 fetch——artifact 是模型写的代码，不该有 Miyu 自己的数据的读取权。
4. **CSP 的 `sandbox` 指令要和 iframe 属性同步放开。** 两者取交集，只改一边等于没改。
5. `{origin}` 而不是 `'self'`：不透明源下 `'self'` 匹配不上任何东西，必须写死 origin。这一条是坑，写错了本地库也加载不了。
   origin 从请求的 `Host` 头拼；反代场景下 scheme 取 `X-Forwarded-Proto`，取不到就按 `http`。

顺带确认过一条实现前提：`/vendor/*` 那几条路由走的是 `embedded_asset`，**不过 `require_auth`**
（只有 ETag 协商）。所以不透明源的 iframe 取得到本地库——§3.1 的货架方案在鉴权上是通的。
如果哪天给 vendor 加了鉴权，货架会连带失效，那时要给它留一条免鉴权的白名单。

### 2.2 预演实测

不改仓库、不重编译，用 Playwright 在路由层把 `app.js` 的 sandbox 属性和后端下发的 CSP 换成候选值，跑同一份探针：

| 探针 | 现状 | 放开后 | 结论 |
|---|---|---|---|
| A 内联 CSS | 生效 | 生效 | — |
| B 内联脚本 | NOT RUN | ✅ **RAN** | 交互活了 |
| F 按钮 `onclick` | 死 | ✅ **CLICKED** | 事件处理器活了 |
| C 外部 CDN 脚本 | 挡 | ✅ 仍然挡 | CSP `script-src` |
| D 外链图片 | 挡 | ✅ 仍然挡 | CSP `img-src` |
| G `fetch('/api/config')` | 脚本没跑 | ✅ **BLOCKED** | CSP `connect-src 'none'` |
| H `window.open('https://外站')` | 脚本没跑 | ✅ **BLOCKED** | 无 `allow-popups` |
| I `<form action="外站">.submit()` | 脚本没跑 | ✅ **BLOCKED** | 无 `allow-forms` |
| J `top.location = '外站'` | 脚本没跑 | ✅ **BLOCKED** | 无 `allow-top-navigation` |

再看隔离的另一半——沙箱里能不能**读到**这边的东西：

| 探针 | 放开脚本后 |
|---|---|
| K `document.cookie` | ✅ **THREW SecurityError** |
| L `localStorage.setItem` | ✅ **THREW SecurityError** |
| M `parent.document.title` | ✅ **THREW SecurityError** |

这三条是不给 `allow-same-origin` 换来的：iframe 拿的是不透明源，浏览器**根本不给它 cookie 和
存储的访问权**，父页面 DOM 更是跨源。

**所以 Miyu 不需要像别家那样再开一个独立域名/端口来隔离 artifact。** 那是给「给了
`allow-same-origin` 的产品」用的补救——它们的 artifact 要跑完整 React、要 localStorage 存状态，
只能保留同源能力，于是被迫用独立注册域把 cookie 域切开（Vercel 甚至把 `vusercontent.net`
送进了 Public Suffix List）。Miyu 的 artifact 是展示型页面，不需要持久状态，
**可以直接走更狠的那条路：连同源资格一起不给**。

代价要写明白：artifact 里用不了 `localStorage` / `IndexedDB`，页面刷新后状态归零。
展示型页面（图表、看板、报告、diff 走查）不受影响；真要做「记得住的小工具」才会撞到。

> 探针 I 的 JS 变量会自报 `SUBMITTED`——`form.submit()` 被拦时不抛异常。真相在控制台：
> `Blocked form submission to 'https://example.com/steal?q=secret' because the form's frame is sandboxed and the 'allow-forms' permission is not set.`
> 判定外带这种事，别信被测代码的自述，信浏览器的。

**一句话：交互全活，外带六条路全死。** 这正是本地单机部署该要的组合——她写的页面能动，但读不到你的东西，也送不出去。

### 2.2b 两个引擎都跑了

**用户本机只有 Firefox（没有 chromium）**，而 Firefox 不实现 Private Network Access，
沙箱与外带的判定未必和 Chromium 一致，所以两边都过了一遍：

| 探针 | Chromium 151 | Firefox 153 |
|---|---|---|
| 内联脚本 | RAN | RAN |
| `document.cookie` | SecurityError | SecurityError |
| `localStorage` | SecurityError | SecurityError |
| `parent.document` | SecurityError | SecurityError |
| fetch 本机 API | BLOCKED | BLOCKED |
| CDN 脚本 | BLOCKED | BLOCKED |
| 外链图片 | BLOCKED | BLOCKED |
| `window.open` | BLOCKED（返回 null） | THREW（抛异常） |
| top 导航 | BLOCKED | BLOCKED |
| 本机 vendor 库 | LOADED（**要 PNA 头，见 3.1**） | LOADED（Firefox 无 PNA，本来就通） |

结论一致，只有 `window.open` 的失败形态不同（一个返回 null 一个抛异常），不影响判定。

复现：
```
BIN=~/.local/bin/miyu WEB=./web python3 testkit/webui-artifact/run.py                        # 现状基线
BIN=~/.local/bin/miyu WEB=./web PROPOSED=1 VENDOR_PNA=1 python3 testkit/webui-artifact/run.py # 放开后
BROWSER=firefox ... 同上                                                                      # 换引擎
```

### 2.3 真实点击那条疑点，已经结了

调研阶段有个悬案：Playwright 的**真实鼠标点击**点不到 iframe 里的按钮（`TimeoutError`），
只有 `dispatch_event('click')` 能触发 `onclick`。当时怀疑是 `.artifact-frame` 的
`zoom: var(--artifact-content-scale)`（实测 1.09）打偏坐标，**证伪**了（zoom 改成 1 照样点不到）。

施工后按真实二进制重跑，同一条 `page.mouse.click`（自己按 zoom 换算屏幕坐标）**点中了**，
按钮文字从 `click me` 变成 `CLICKED`。所以：

- **交互是真的可用**，不是只有合成事件能跑。
- Playwright 自带的 `locator.click()` 仍然超时——它的 actionability 检查（elementFromPoint
  那套）在不透明源被 site isolation 拆成独立进程（OOPIF）之后就不工作了。**这是测具的限制，
  不是产品缺陷**，判定要用自算坐标的 `mouse.click`。

---

### 2.3b 一条封不住的残余通道：WebRTC

Claude 生产环境的 artifact CSP 里有一条 `webrtc 'block'`——WebRTC 能绕开 `connect-src` 建立
出站连接，是外带的老通道。我照抄了这条，然后实测：

| CSP | Chromium 151 | Firefox 153 |
|---|---|---|
| 不写 `webrtc 'block'` | `RTCPeerConnection` 构造成功 | 构造成功 |
| 写了 `webrtc 'block'` | **构造照样成功** | **构造照样成功** |

自查过不是我的头没发出去——同一个响应头里的 `connect-src 'none'` 明明生效了（fetch 被拦）。
控制台给了答案：

```
Unrecognized Content-Security-Policy directive 'webrtc'.
```

**两个引擎都不认这条指令。也就是说 Claude 那条 `webrtc 'block'` 在当前浏览器上同样是失效的**，
它防的是将来。

所以要如实记下来：**放开脚本之后，WebRTC 是一条 CSP 封不住的残余外带通道。** 权衡时要看清它的
实际权重——artifact 是她自己写的代码，威胁模型是 prompt injection 诱导她写出外带代码；而一个
已经被诱导的模型手里有 `web_fetch`、`run_command` 这些**更直接**的外带手段。artifact 沙箱防的
是「页面被打开时的自主行为」，从来不是「防住一个已经被完全控制的模型」。这条通道不改变那个判断，
但不能假装它不存在。

指令仍然建议写上（无害，只多一条控制台警告，将来浏览器支持了就自动生效）。

### 2.4 更正一条：WebUI 主页面**是有** CSP 的

调研阶段我写过「全站只有 artifact 那一条 CSP」——**错的**。我当时只 grep 了字面量
`Content-Security-Policy`，没搜 `CONTENT_SECURITY_POLICY` 这个常量名。

实际上所有静态资源（index.html 在内）都过 `finish_asset_response`，那里统一下发：

```
default-src 'self'; img-src 'self'; media-src 'self' https: http:; style-src 'self';
script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'
```

对本方案没有影响：`frame-src` 未指定会回退到 `default-src 'self'`，而 artifact 的 iframe
指向同源的 `/api/artifacts/…`，放行。**但如果哪天 artifact 要挪到独立 origin，这条会挡住**，
到时得同步加 `frame-src`。

---

## 三、配套项（按推荐优先级）

### 3.1 本地库货架 —— 让"画个图表"真的画得出来

放活脚本之后，模型写 `<script src="https://cdn.../chart.js">` 仍然会被 CSP 挡，而这是它最想写的东西。CDN 白名单不能开（开了就等于开了外带通道）。**唯一干净的解法是把库放本地。**

Miyu 已经有这个先例：KaTeX 和 Prism 就是 vendored 的（`web/vendor/`，`include_str!` 编进二进制，`/vendor/...` 路由发出去）。加库就是照抄一遍。

体积实测（cdn.jsdelivr.net 拉的真实字节，二进制现值 62.9 MB）：

| 库 | 原始 | gzip | 二进制增幅（原始 / gzip 预压缩） |
|---|---:|---:|---|
| Chart.js 4 | 203 KB | 68 KB | +0.32% / +0.11% |
| D3 7 | 273 KB | 90 KB | +0.42% / +0.14% |
| ECharts 5 | 1009 KB | 326 KB | +1.6% / +0.51% |
| Mermaid 11 | 3488 KB | 952 KB | +5.5% / +1.5% |

> **gzip 预压缩**：仓库里存 `.js.gz`，`include_bytes!` 原样编进去，HTTP 层直接带
> `Content-Encoding: gzip` 发出（服务端不解压、浏览器自己解）。体积按 gzip 列算。
> 这对 low-footprint 那条线是划算的——但它是个新花样，要单独验一次。

**这里有个本地部署特有的坑，实测撞到的。** 第一次试的时候本地库根本加载不上：

```
Access to script at 'http://127.0.0.1:18485/vendor/prism/prism.min.js' from origin 'null'
has been blocked by CORS policy: Permission was denied for this request
to access the `loopback` address space.
```

不透明源被 Chromium 当成**公网源**，而目标是 loopback，属于跨地址空间访问，被 Private Network
Access 拦下——**CSP 里 `{origin}` 写得再对也没用，PNA 拦在更前面**。竞品调研里没有这条，因为
Claude/ChatGPT/LibreChat 全是公网部署，撞不到 loopback 这个场景。

解法是给 `/vendor/*` 路由补两个响应头，并处理 `OPTIONS` 预检（PNA 会为跨地址空间请求强制预检）：

```
Access-Control-Allow-Origin: *
Access-Control-Allow-Private-Network: true
Access-Control-Allow-Methods: GET, OPTIONS
```

加上之后实测 `local-vendor: LOADED (Prism ok)`，Chromium 和 Firefox 都通。

两条注意：
- `Allow-Origin: *` 只能给 `/vendor/*` 这类公开静态库（MIT 许可的第三方 JS，没有隐私），
  **绝不能加到 `/api/*` 上**。
- PNA 正在被 Chrome 换成 Local Network Access 的权限提示模型，这个头将来可能失效。
  它是浏览器策略，会变——所以要留一条降级路径（库加载不上时页面仍能显示，别整页白屏）。

模型怎么知道货架上有什么？写进 `artifact.json` 的描述里（常量字节，不违反缓存契约；改描述 = 一次性计划内冷启动，AGENTS.md §1.6 认可）。

### 3.2 类型缺口补齐

| 项 | 做法 | 备注 |
|---|---|---|
| SVG 预览 | `artifact_media_type` 认 `svg` → 新 kind；预览**必须走沙箱 iframe** | SVG 是活性内容（能带 `<script>`），绝不能用 `<img>` 之外的方式直接进主文档 |
| CSV/TSV 表格视图 | 前端解析成 `<table>`，与源码视图并列 | 纯前端，无后端改动 |
| 源码视图接高亮 | `renderArtifactSource` 调 `MiyuHighlight.paint` | 现成组件，聊天正文已经在用 |

> 调研时我把「源码长行看不见」也列成了缺口，**是错的**。`.artifact-source` 是
> `width: max-content` 配行号列 sticky，长行走的是横向滚动——标准代码查看器行为，
> 不是截断。这条不用改。

### 3.2b 一份 HTML 引不到另一份 artifact

她生成一张图 + 一份引用这张图的 HTML 报告——现在做不到。artifact 的 URL 是
`/api/artifacts/art_<hash>`，HTML 里写 `<img src="chart.png">` 会解析成
`/api/artifacts/chart.png`，而那条路由的 id 校验只放行 `[A-Za-z0-9_-]`（点号直接出局），
稳定 404。她也无从得知另一份的 `art_` id。

同样的问题波及她自己生成的图（`generate_image`）和用户上传的附件——HTML 报告引用不到任何
一张已经存在于这个会话里的图。

两条路：

- **按名字寻址**（推荐）：加一条 `/api/artifacts/by-name/{session_id}/{file_name}`，
  让相对路径自然可用。原样交付，模型按直觉写就对。
- 前端重写引用：读 HTML 源码把相对路径换成 `art_` id 再塞进 iframe。破坏「原样交付」，
  而且下载下来的文件和看到的不是一份，不推荐。

注意按名字寻址那条路要重新过一遍路径逃逸（`..`、绝对路径、控制字符），
`managed_file_path` 里现成的那套判定可以复用。

### 3.3 Mermaid —— 走同一条沙箱通道

markdown 里的 ` ```mermaid ` 现在按普通代码块渲染。想让它变成图，**不能在主页面跑 mermaid**：

> `web/highlight.js` 的头注释写得很清楚——「整个 markdown 渲染器是刻意的纯 DOM 实现，
> 模型写出来的字永远不该有机会变成标记」。mermaid 内部大量 `innerHTML`，历史上有过 XSS，
> 把它放进主文档等于亲手拆掉这条红线。

正确做法是让它复用 §2 这条已经建好的沙箱通道：**mermaid 围栏 → 一个 sandboxed iframe，里面加载本地 mermaid + 图定义。** 这样「活性内容一律进沙箱」成为一条统一规则，HTML artifact、mermaid 图、将来的图表都走同一个门。

代价是 3.4 MB（gzip 952 KB）的体积，是本文档里最贵的一项，**建议单独拍板**。

### 3.4 面板本身的交付力

| 项 | 价值 | 成本 |
|---|---|---|
| 「在新标签页打开」按钮扩到 HTML/PDF/MD | 高：全屏看长报告 | 极低（图片已有这个按钮） |
| 版本历史 / 回滚 | 中：她改坏了能退回去 | 中：要给 `artifact_assets` 加版本表 |
| 面板内编辑 → 回传给她 | 中：改一行不用重说一遍 | 中：要开写路由 + 回合注入 |
| 导出 PDF | 中：走浏览器打印即可 | 低 |
| artifact 分享链接 | 低：本地部署场景弱 | 中：要独立的免鉴权令牌 |
| 面板里选中一段 → 引用回输入框问她 | 中：改稿闭环，省一遍复述 | 低（markdown/源码视图内选区即可；HTML 预览是不透明源，取不到选区） |
| 流式预览（她写的时候就能看） | 低：artifact 工具是一次性落盘，现在没有中间态 | 高：要改工具的产出协议 |

### 3.5 她得知道自己有这个能力（最便宜的一条）

系统提示词里**一个字都没提 artifact**，工具描述里也只说「create or edit deliverable files」——没说可以写交互式 HTML，更没说有哪些库可用。

能力再强，她想不起来用就等于没有。工具名是最强的能力广告（AGENTS.md §2.2），描述是第二强的。这一条改动量最小、见效最快，但**必须等 §2 落地之后再改**——不然是广告一个不存在的能力。

### 3.6 QQ 端（单独议题）

QQ 侧完全没有 artifact 概念。她在 QQ 里做完一份报告，用户拿不到。可选路子：`share_file` 给链接 / 长文转图 / 直接发文件。这条牵涉 QQ 线的设计，建议与本文档分开。

---

## 四、推荐的施工顺序（§6 记录了实际落地范围）

1. **P0 §2 放活 HTML**（核心诉求，已预演实测，改动最小收益最大）
2. **P1 §3.2 类型缺口**（SVG / CSV / 源码高亮，纯补齐，风险低）
3. **P2 §3.1 本地库货架 + §3.5 描述广告**（让"画个图表"真能画）
4. **P3 §3.4 新标签页打开 + 导出**（低成本高频）
5. 待拍板：§3.3 mermaid（3.4 MB）、§3.4 版本历史 / 面板内编辑、§3.6 QQ 端

---

## 五、竞品对照（佐证材料）

单独调研了 Claude.ai / ChatGPT / Open WebUI / LibreChat / v0 的做法。**结论：本文档 §2 的方案
比它们全部更严格，而且是有意为之。** 下面只列对决策有用的部分，每条标了把握度。

### 5.1 sandbox 属性：为什么别家都给 `allow-same-origin`，我们不给

| 产品 | 隔离方式 | sandbox 属性 |
|---|---|---|
| Claude.ai / Desktop | **独立注册域** `www.claudeusercontent.com` | `allow-scripts allow-same-origin`（逆向得来，把握 80%，无交叉验证） |
| ChatGPT Apps widgets | 独立注册域 `*.web-sandbox.oaiusercontent.com` | CSP `sandbox` 指令：`allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms`（实测 95%） |
| Open WebUI | `srcdoc` 同域文档，靠 sandbox 拿不透明源 | 默认 `allow-scripts allow-downloads allow-forms`（**不给 same-origin**） |
| LibreChat | 跨域 CodeSandbox | Sandpack 写死 `... allow-same-origin allow-scripts ...` |
| **Miyu（本方案）** | **同域，不透明源** | **`allow-scripts allow-modals`** |

它们给 `allow-same-origin` 是因为要跑完整 React、要 `localStorage` 存状态——保留了同源能力，
就**必须**用独立注册域把 cookie 域切开（Vercel 甚至把 `vusercontent.net` 送进了 Public Suffix
List）。规范说得很直白：

> Setting both `allow-scripts` and `allow-same-origin` **when the embedded page has the same
> origin as the page containing the iframe** allows the embedded page to simply remove the
> `sandbox` attribute and reload itself, effectively breaking out of the sandbox altogether.
> —— WHATWG HTML spec

Miyu 的 artifact 是展示型页面，不需要持久状态，所以能走更狠的那条：**连同源资格一起不给**，
于是也不需要独立域。这在 §2.2 的三条 `SecurityError` 里已经实测坐实。

### 5.2 别人踩的坑，正好是我们避开的那条

Open WebUI 有**五个 High 级 CVE 是同一个 bug 家族**：某处 iframe 把 `allow-same-origin` 写死、
绕过了用户开关，而内容由应用自身 origin 提供——sandbox 属性成了唯一防线，而它恰好被自己撤销了。
CVE-2026-87995 的 advisory 原话：

> the sandbox was the only isolation boundary left, and it was **granting precisely the
> permission that dissolved it**

（CVE-2026-26192 / 26193 / 70486 / 87995 / 59214，把握 95%，取自 GitHub Security Advisory 原文）

LibreChat 的 **CVE-2026-54025** 则印证了另一半：它的沙箱是跨域的，DOM 和 cookie 都隔离住了，
但 `window.top.location = 'https://attacker.com'` **顶层导航劫持穿透了**——因为 LibreChat
不下发 CSP 也不下发 X-Frame-Options。本方案不给 `allow-top-navigation`，这条已实测 BLOCKED。

**还有一条被 advisory 原文确认的反直觉事实**（CVE-2026-59214 修复说明）：改成不透明源之后
"Full Python, JavaScript and **external fetch** keep working"——**sandbox 属性解决「权限」，
不解决「外泄」；外泄只能靠 CSP。** 这正是本方案两者都用的原因。

### 5.3 Mermaid：三家三种跑法，我们该抄哪种

| 产品 | mermaid 库跑在哪 | 风险 |
|---|---|---|
| **Open WebUI** | **主文档里**，`securityLevel:'loose'` 出原始 SVG，靠 DOMPurify 兜 | 最高：DOMPurify 是唯一防线，绕过就是主源 XSS |
| **LibreChat** | 沙箱 iframe 内（Sandpack 依赖） | 低 |
| **Claude** | 自己打包 mermaid 渲染出 SVG，再塞进 `data:` 的**二级 iframe**，二级 CSP 连脚本都不给 | 低 |

本文档 §3.3 选的是 LibreChat/Claude 那条（进沙箱），**明确不抄 Open WebUI 那条**——把
mermaid 放进主文档正是 `web/highlight.js` 头注释那条红线禁止的事。（把握 90~95%，均为源码）

### 5.4 库从哪来：他们靠 CDN 白名单，我们只能靠本地货架

Claude 的 artifact CSP 放行了 `cdnjs.cloudflare.com`、`cdn.jsdelivr.net/npm/`、
`cdn.tailwindcss.com`、`code.jquery.com` 四个脚本源（实测 100%），**零内置库**；
`img-src` 除 `data:`/`blob:`/`self` 外只放行了 OpenStreetMap 瓦片，`connect-src` 打不到任何
第三方 API——**掐出站的思路和本方案一致**，只是他们有公网可依赖。

LibreChat 走另一个极端：把 recharts / three.js / 30+ 个 Radix UI / shadcn 全套**打包**进
Sandpack 依赖。

Miyu 本地单机部署，不该假设有公网，所以走本地货架（§3.1）——形态上更接近 LibreChat，
但只放几个真正用得上的。

### 5.5 多文件引用：这是各家都没解决的问题

Claude 官方文档明说 artifact 必须**单页**：

> Relative links do not resolve… uses in-page anchors rather than separate files

所以 §3.2b 那条（一份 HTML 引不到另一份 artifact）如果做了，是**超出竞品**的能力；
反过来说，它的优先级也可以相应放低——各家都没做，说明不是刚需。

### 5.6 一个反向信号，但不构成依据

ChatGPT 于 2026-05-28 在 GPT-5.5 上**移除了 Canvas**，改成聊天流内联的 writing blocks /
code blocks（官方 release notes 原文，把握 95%）。但同期 Anthropic 在把 artifact 面板往更重
的方向做（连接器、持久化存储、多人协同）。两家分叉，**不构成「Miyu 也该砍掉右侧分栏」的依据**。

值得抄的是 ChatGPT 企业版那个交互：预览要连未知第三方时**逐次弹窗让用户确认**，比一刀切
allow/deny 友好。不过在本方案的 `connect-src 'none'` 下暂时用不上。

---

## 六、施工记录（2026-09-11 落地）

用户拍板：**通电 + ECharts + 顺手修三个小毛病**。分支 `artifact-live`。

### 6.1 实际改了什么

| 文件 | 改动 |
|---|---|
| `web/app.js` | iframe `sandbox` 放开到 `allow-scripts allow-modals`；csv 表格视图；源码视图接 `MiyuHighlight`；模式切换器的显隐判据从「是不是图片」换成「有没有得切」 |
| `src/web/assets.rs` | `artifact_csp()` 按 kind 下发策略（html 放脚本掐出站 / svg 维持最严）；`request_origin()` 从 Host 头拼来源并做白名单校验；`allow_sandboxed_frames()` 补 CORS + PNA 头；`vendor_gzip_asset()` 直发 gzip |
| `src/web/server.rs` | 每条 `/vendor/` 路由配 `options` 分支（PNA 预检）；新增 echarts 路由 |
| `src/web/mod.rs` | `ECHARTS_JS_GZ` 的 `include_bytes!` |
| `src/state/mod.rs` | `artifact_media_type` 认 svg / csv / tsv |
| `src/tools/descriptions/artifact.json` | 告诉她 HTML 会在沙箱里跑、有哪几个本地库、外网是断的 |
| `web/vendor/echarts/echarts.min.js.gz` | ECharts 6.1.0，Apache-2.0，gzip 存（1096 KB → 359 KB） |

二进制 62.9 MB → **63.3 MB**（+388 KB）。

### 6.2 真机验收数据

沙箱 daemon + 真实 release 二进制（不是路由层预演），Chromium 151：

| 项 | 结果 |
|---|---|
| 内联脚本 | RAN |
| 按钮真实鼠标点击 | **CLICKED** |
| **ECharts 从本机货架加载并出图** | **RENDERED 504×288，两个 canvas** |
| 本机 vendor 库（Prism） | LOADED |
| cookie / localStorage / 父页面 DOM | 三条全 SecurityError |
| fetch / window.open / form / top 导航 | 四条全 BLOCKED |
| CDN 脚本 / 外链图片 | 两条全 BLOCKED |
| svg 预览 | 出图，且切换器/缩放/复制齐全 |
| csv 表格 | `tables=1` |
| 源码高亮 | html `tokenSpans=1120`、md `22` |

两个引擎都跑了（用户本机是 Firefox），结论一致，只有 `window.open` 的失败形态不同
（Chromium 返回 null，Firefox 抛异常）。

**反向验证**：同一套判定拿改之前的二进制配上从 HEAD 取出的旧前端跑一遍，**3/16**
（改后 16/16），13 条报红。判据守得住。

有一点要讲清楚：「箱子密封」和「外带堵死」那几条在旧版本上也显示未达标，**不是因为旧版本漏了，
而是因为脚本压根不跑、探针执行不到**。这恰好点破了两种安全的区别——旧版本的安全来自
「什么都不许动」，新版本的安全来自「逐条封死」。前者顺带把功能一起噎死了。

后端实际下发的 CSP（从真 daemon 抓的）：

```
sandbox allow-scripts allow-modals; default-src 'none';
script-src 'unsafe-inline' 'unsafe-eval' http://127.0.0.1:18485;
style-src 'unsafe-inline' http://127.0.0.1:18485; font-src data: http://127.0.0.1:18485;
img-src data: blob: http://127.0.0.1:18485; media-src data: blob: http://127.0.0.1:18485;
connect-src 'none'; form-action 'none'; frame-src 'none'; object-src 'none';
base-uri 'none'; webrtc 'block'
```

### 6.2b 量尺

```
# 16 项判定 + 截图，两个引擎都能跑
BIN=<二进制> WEB=./web python3 testkit/webui-artifact/run.py
BROWSER=firefox BIN=<二进制> WEB=./web python3 testkit/webui-artifact/run.py

# gzip 直发 / 现解 / PNA 预检三项（这次唯一的新花样，单独立一道）
BIN=<二进制> python3 testkit/webui-artifact/vendor_headers.py

# 手动验收沙箱：起隔离 daemon 打印地址，人手点那个按钮
BIN=<二进制> python3 testkit/webui-artifact/manual.py
```

`run.py` 还留着 `PROPOSED=1` 的预演模式（在路由层替换 sandbox 属性和 CSP），
将来要再动这两个值，可以先不重编译预演一轮。

### 6.3 施工中撞到的两件事

**一、测具判据一开始就是错的。** 高亮的 span 类名是 `tok-<角色>`（`web/highlight.js` 自己映射的），
不是 Prism 原生的 `.token`。基线那轮查 `.token` 得 0，恰好和「确实没上色」的事实撞了个正着，
所以没暴露。改完之后仍是 0，才发现判据从头到尾都没在测该测的东西。**两个 0 不是同一个 0。**

**二、数据全绿，截图里却少了半个工具条。** svg 的 mime 是 `image/svg+xml`，被
`isImage` 判定拖去走纯图片分支，整组「预览/源码」切换器的**父元素**被藏了——而报告只查了
按钮自身的 `hidden`，所以一片绿。是截图看出来的。判据已经补成查父元素，回归守得住了。

---

## 七、待拍板清单

0. **WebRTC 那条残余通道（§2.3b）接受吗？** 放开脚本就必然带着它，CSP 现在封不住。
   我的判断是接受——被诱导的模型手里有更直接的外带手段，这条不改变威胁模型。
1. §2 的 sandbox 只给 `allow-scripts allow-modals`，代价是 artifact 里的 `<a target="_blank">` 点不开。接受吗？（备选：父页面拦 postMessage 代开，是额外工作量）
2. §3.1 货架上放哪几个库？（Chart.js 最便宜，ECharts 最全，D3 最灵活）
3. §3.1 用不用 gzip 预压缩这个新花样？
4. §3.3 mermaid 的 3.4 MB 花不花？
5. §3.4 里哪些要做？
6. §3.6 QQ 端这条现在开不开？
