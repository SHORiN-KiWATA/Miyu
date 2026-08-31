# bilibili_live_stream —— 给 AI 用的哔哩哔哩直播控制脚本

单文件 CLI，同时是 Miyu script 工具（stdin 传 JSON、stdout 出纯文本或 JSON）。
**只依赖 Python 标准库**，Python ≥ 3.8——二维码是自己生成的，没有 `qrcode` / `Pillow` 依赖。

接口逻辑取自 [bili-live-hime](https://github.com/Rsplwe/bili-live-hime)：APP 签名见
`src/lib/app-sign.ts`，开播与改标题/分区见 `src/api/live.ts`。

## 用法

```bash
bilibili_live_stream login                        # 登录，调两次（下面单独说）
bilibili_live_stream status                       # 直播间状态（只读）
bilibili_live_stream areas                        # 列父分区（只读，不用登录）
bilibili_live_stream areas --search 虚拟          # 在子分区里搜
bilibili_live_stream areas --parent 网游          # 列某父分区下的子分区
bilibili_live_stream update --title "新标题"      # 改直播标题
bilibili_live_stream update --area 虚拟日常       # 改分区
bilibili_live_stream update --title X --area Y    # 一起改（一次请求，见下）
bilibili_live_stream start                        # 开播，返回推流地址与推流码
bilibili_live_stream stop                         # 下播
```

AI 侧走 stdin JSON，字段名就是去掉 `--` 的选项名：

```bash
echo '{"command":"status"}'                                  | bilibili_live_stream
echo '{"command":"areas","search":"我的世界"}'                | bilibili_live_stream
echo '{"command":"update","title":"新标题","area":"虚拟日常"}' | bilibili_live_stream
echo '{"command":"start","json":true}'                        | bilibili_live_stream
```

默认输出**精简纯文本摘要**（省 token，不带任何 markdown 修饰）；要完整结构化数据加
`--json` / `"json":true`，成功是 `{"ok":true,"command":...,"data":{...}}`，失败是
`{"ok":false,"command":...,"error":{...}}`。两种模式都写 stdout，成功退 0、失败退 1。

## 分区怎么写

`--area` 四种写法都认，脚本自己解析成 id：

| 写法 | 例子 |
|---|---|
| 数字 id | `371` |
| 子分区名 | `虚拟日常` |
| 拼音 | `xunirichang` |
| `父分区/子分区` | `虚拟主播/虚拟日常` |

精确名和拼音**优先于**子串匹配，所以 `明日方舟` 命中 255 而不是「明日方舟：终末地」。
匹配到多个时报 `usage` 错误，候选放在 `error.detail.candidates` 里——拿去问用户，
或者直接换成 id 重试。全站 12 个父分区、442 个子分区（2026-08-29 实测），
列表缓存一天，`--refresh` 强制刷新。

## login 是两步的，每次都秒回

```
第一次调 → status=pending，返回二维码图片路径 qr_image。把图片交给用户去扫。
第二次调 → status=success 完成登录。用户其实没扫完就返回 waiting，二维码不变，
           等他扫完再调一次即可。
```

之所以不做成"一次调用等着用户扫"，是因为阻塞的命令会把整个工具调用卡住几十秒，
期间既看不到输出、也没法把二维码递给用户。真要阻塞可以加 `--wait 秒数`，
但那只适合有人盯着实时终端的场景。

二维码**一律写成 PNG 文件并返回路径**，脚本自己绝不往终端画任何图形——
怎么呈现给用户由调用方决定。

## error.kind 是稳定的分支依据

| kind | 含义 | 下一步 |
|---|---|---|
| `auth` | 没登录或登录态失效 | 引导用户跑 `login` |
| `verify_required` | 开播需 App 扫码验证 (60024) | 图片在 `error.detail.qr_image` |
| `face_auth_required` | 开播需人脸认证 (60043) | 图片在 `error.detail.qr_image` |
| `usage` | 参数问题（分区名歧义、没设分区、没开通直播间…） | 按 `hint` 改参数重试 |
| `api` / `network` / `http` | 接口报错 / 网络 / 协议 | 见 `hint`，常见 code 有专门提示 |

## 实测结论（2026-08-29，真账号真直播间）

| 问题 | 实测结果 |
|---|---|
| 标题和分区能分两次请求改吗？ | **不能**。参考实现 `bili-live-hime` 的 `updateRoomTitle` / `updateRoomArea` 是两次 POST，实测第二次直接被挡成 `操作太频繁 (-1)`——标题改了、分区没改，留下改一半的状态。 |
| 那怎么办？ | 这俩本来就是同一个接口 `/room/v1/Room/update` 的不同字段，**合成一次请求发**。已改成 `update_room(title=, area_id=)`。 |
| 合并请求是原子的吗？ | **是**。撞上 60024 那次，分区没改、标题也没改，整个请求被一起打回。 |
| 分区改动会立刻生效吗？ | 会，**在播时也会**，观众马上看到。所以 `update` 在直播中会返回 `warning`。 |
| 所有分区都能用吗？ | **不是**。虚拟主播区要账号做过人脸认证，改过去返回 `60024 目标分区需要人脸认证`。另有被 B 站 `lock_status` 锁掉的分区（如"虚拟男V"），脚本在发请求前就拦掉。 |
| 分区列表带拼音吗？ | 带。`getList?show_pinyin=1` 的 `pinyin` 字段可直接用来匹配，`xunirichang` → 371 实测命中。 |
| 改设置有频率限制吗？ | 有。连着两次 `update` 会撞 `-1`，间隔十几秒就正常。 |

跑通的往返：标题改了又改回、分区 `1083 → 255 → 1083`、`1083 → 107 → 1083`（跨父分区），
每步都用 `status` 向服务端复核，最后完全恢复原状。

## 已知限制

- **`start` 是不可静默撤销的对外动作**。直播间立刻对外可见、粉丝收到开播推送。
  调用前必须获得用户明确同意——这条写进了脚本头部描述和 `index-entry.json`，
  就是为了让模型在调之前先问。
- **推流码是敏感凭据**。别转发给第三方服务，也别写进日志。
- **凭据只有一个来源**：`login` 写、`logout` 删。缓存目录默认
  `~/.cache/bilibili-live-stream/`（尊重 `XDG_CACHE_HOME`，Windows 用 `LOCALAPPDATA`，
  macOS 用 `~/Library/Caches`），`credentials.json` 权限 0600。
  `BILIBILI_LIVE_STREAM_HOME` 可换位置。
- **脚本改名前的缓存会自动搬迁**。老目录 `~/.cache/bilibili-stream-key/` 存在且新目录
  不存在时，首次运行整个 `os.rename` 过去，不用重新登录；老环境变量
  `BILIBILI_STREAM_KEY_HOME` 仍然认。
- **一个账号只有一个直播间**，脚本用 `nav` 拿 uid 再换 room_id，没开通直播间会报 `usage`。

## 设计要点

- **标题和分区合成一次请求**，不是为了少发一个包，是因为分两次会被限流打成半成品状态。
  这是这次改动里唯一一处**故意偏离参考实现**的地方。
- **先解析分区名，再动直播间**。`resolve_area()` 在任何 HTTP 写操作之前跑完，
  分区名写错就直接报错返回，不会留下"标题改了分区没改"。
- **锁定分区在本地拦掉**。`lock_status != 0` 的分区不发请求，省一次必然失败的往返。
- **二维码只写文件不画终端**。画终端的话，调用方（AI）看到的是一堆方块字符，
  既没法转交用户、又白占上下文。
- **默认输出是纯文本不是 markdown**。这脚本的输出会原样进模型上下文，
  markdown 修饰在这儿只是纯粹的 token 开销。
- **`--json` 放在 parent parser 且用 `SUPPRESS` 作默认值**，所以 `--json status` 和
  `status --json` 都能用，子命令的默认值也不会把前置选项悄悄覆盖掉。

## 目录

```
bilibili_live_stream   单文件脚本(CLI + Miyu 工具)
index-entry.json       Miyu src/scripts/index.json 的注册项(尚未安装)
```
