# Miyu 语音功能(v2):唤醒、识别、听写

> 2026-09-05 重做。v1(08-14,voice 分支)只当参考件,设计取舍见
> `docs/plan/2026-09-05-voice-v2.md`。本文是现状:怎么装、怎么用、怎么排查、
> 各部件住在哪、实测数字。**不做 TTS**(语音回复走桌面通知 + 提示音)。

## 一、一句话

装上可选组件 `miyu-voice`,设置里开「语音功能」,daemon 会拉起一个独立进程
常开麦克风:喊「未有未有」→ 提示音 + 通知「Miyu 在听」→ 说指令 → 通知「Miyu
收到:…」→ 她在「语音会话」里执行 → 完成后提示音 + 通知回复摘要。终端 REPL
`/stt`、`miyu stt`、WebUI 麦克风按钮三个入口共用同一套识别做**听写**。

## 二、为什么是两个可执行

| | `miyu`(主程序) | `miyu-voice`(语音前端) |
|---|---|---|
| 含 sherpa-onnx / onnxruntime | 否 | 是(静态链接,二进制 ≈ +30MB) |
| 打开麦克风 | 否 | 是 |
| 识别模型 | 否 | VAD + 唤醒词常驻,SenseVoice 按需加载/闲置卸载 |
| 不开语音时的占用 | 零 | 不会被拉起 |

主程序对语音的全部认知在 `src/web/voice_bridge.rs`:找二进制(同目录 → PATH)、
拉起/看护(崩溃退避重启、配置重载时重启、daemon 退出收走)、一条持久 IPC 信令
连接、语音回合驱动、通知、听写中继。`miyu-voice` 只懂音频:麦克风 → 能量门 →
VAD → 唤醒词 → 识别 → 信令;它以普通 IPC 客户端连回 daemon,daemon 消失即退出。

```
miyu(daemon) ──spawn──> miyu-voice
   voice_bridge  <──VoiceAttach 双向 Event 帧──>  voice::worker
   · voice.command{text} → StartTurn(语音会话 lane) → 通知/cue(done)
   · voice.speech_start → Cancel(打断)
   · voice.dictation{text} → 听写认领者(REPL /stt、miyu stt)
   · voice.transcribe{wav_path} ← WebUI 浏览器录音
```

Cargo:`voice` feature **默认关**;`[[bin]] miyu-voice` 标 `required-features`。
`cargo build --release` 只出 miyu;`cargo build --release --features voice` 两个都出。
打包:`packaging/arch/miyu-release` 拆成 `miyu` + `miyu-voice` 两个包,sherpa 静态库
作为 source 由 makepkg 下载校验(`SHERPA_ONNX_ARCHIVE_DIR`),构建期不联网;
AUR 包装包 `packaging/arch/miyu-voice`。

## 三、模型(约 190MB,`state_dir/models/`,首次启用自动下载并通知)

| 模型 | 体积 | 职责 | 常驻? |
|---|---|---|---|
| Silero VAD | 0.6MB | 有没有人在说话 | 是(能量门之后) |
| KWS Zipformer(wenetspeech 3.3M int8) | 31MB | 只认唤醒词的流式小模型 | 是 |
| SenseVoiceSmall int8 | 156MB | 句子→文字(中日英韩粤,非自回归) | 用完按 `stt_unload_seconds` 卸载 |

唤醒词三种写法(`keywords.rs`,不用重训):

| 写法 | 例子 | 处理 |
|---|---|---|
| 汉字 | `未有未有`、`密友密友` | 每字带调全拼按 KWS `tokens.txt` 最长匹配拆分,一行 |
| 假名 / 拉丁 | `みゆみゆ`、`miyumiyu` | 假名→罗马音→按日语音节映射到近似拼音(ゆ 按 you 而非 yu),声调未知就同一个词展开成轻声+一到四声五行候选,任一命中即算 |
| 显式拼音 | `mi3 you3 mi3 you3` | 空格分隔的"拼音+声调数字",原样编码(给想精调的人) |

编不出来的词只记日志跳过,不影响别的词。**模型是普通话模型**,非中文只是
"按中文口音念"的近似:09-05 用 MiniMax 合成样本实测,`密友密友` 五种中文
音色 5/5 命中;`miyumiyu` / `みゆみゆ` 按中文口音念(识别成"米有米游")命中,
**日语母语发音(6 条日语音色)和英语发音(3 条)全部不命中**,一百种声调组合
都试过——要认日语原音得换路线(识别文本兜底或日语唤醒模型),未做。
`wake_threshold`(默认 0.25,越低越灵敏)/ `wake_boost`(默认 1.0,越大越灵敏)
直接对应 sherpa 的 keywords_threshold / keywords_score;这两个旋钮对"发音不像"
没有帮助(阈值降到 0.05、加分 3.0 仍不命中)。

**段前补音(09-05)**:VAD 判"开始说话"总比真实起音晚一点,能量门又把起音前的
弱帧整个跳过,m/n/b 这类弱起音的词头一个音节常被切掉——裸模型能命中、管线
不命中(`密友密友` 0/5 → 补音后 5/5)。管线现在保留最近 23s 原始音频环形缓冲,
每个语音段送 KWS/STT 前从缓冲补回段前 500ms。

## 四、管线与低占用手段(`src/voice/pipeline.rs`)

```
帧(16k mono) → 能量门 → VAD 切段 → [Idle] 整段过 KWS → 命中即发 Wake →
               同段转写:有指令 → Command;没有 → Awaiting(8s 等下一段)
             → [Window] 免唤醒:整段转写 → Command(唤醒对话)/ Dictation(听写)
```

- **能量门**:帧 RMS 低于自适应噪声底×2.5(且 < -54dBFS 绝对下限)时不进 VAD;
  VAD 句中或句尾 1.5s 内照常喂,保证切句时序不变。安静房间 CPU 接近零。
- **KWS 整段判**而非逐帧流式:唤醒和指令常在同一口气里,反正要等说完。
- **Wake 即刻上报**:KWS 命中先发 Wake(提示音/通知),再转写——冷启动加载 STT
  的两秒不挡在"她听到了"前面。
- **STT 闲置卸载**:窗口关闭且闲置 `stt_unload_seconds`(默认 60)后释放约 300MB。
  开听写窗时预加载。
- **过滤**:语音段 < 0.5s 不进 STT;识别文本有效字(字母数字/汉字)< `min_utterance_chars`
  (默认 2)当噪声丢弃,不打扰也不关窗。
- 窗口计时冻结:回合运行到合成结束(`voice.hold`)、播报期间(不喂帧)静默都不
  消耗追问窗口,窗口从播完起算。
- 打断:窗口内持续 0.3s 人声 → `speech_start` → daemon 取消进行中的回合。

## 五、交互

| 事件 | 提示音 | 桌面通知 |
|---|---|---|
| 唤醒词命中 | wake(上行两音) | 「Miyu 在听 / 请讲」(与提示音同一瞬间) |
| 识别出指令 | heard(单点) | 「Miyu 收到 / <指令>」 |
| 回合完成 | done(下行三音) | 「Miyu / <回复前 N 字>」(N=`notify_reply_chars`) |
| `miyu listen` 再按一次关闭 | off(下行两音,wake 的镜像) | 「Miyu / 不听了」 |
| 回合失败 | error(低音) | 「语音会话出错 / …」 |
| 窗口关闭/超时 | 无 | 无 |

**语音会话**:唤醒对话落在一条专属会话(kind = `voice`,id 记在
`state/voice-session-id`),不进 WebUI 列表;用 `miyu voice history [--limit n]`
回看、`miyu voice reset` 清空(下次唤醒重建)、`miyu voice status` 看前端状态。

**回复播报(TTS)**:播报供应商独立于 LLM 的 providers 配置(免得混),预置两家,
`voice.tts.active` 选一个(缺省 MiniMax):

- **MiniMax**:`voice.tts.minimax` 里填自己的 api_key(账号级,对话与语音共用一把,
  支持 `$env:VAR`)、国内/国际站地址、模型、音色、语速/音量/音调/情绪/语种增强。
  合成走 `t2a_v2` 返回 wav。
- **小米 MiMo**(`voice.tts.mimo`,09-06):platform.xiaomimimo.com 的 key(TTS
  系列限时免费),接口是 OpenAI 兼容的 `chat/completions`——待合成文本放
  assistant 消息,风格指令放 user 消息,音频以 base64 wav 回来(24kHz 单声道)。
  三个模型:`mimo-v2.5-tts` 用预置音色(`voice`:mimo_default / 冰糖 / 茉莉 /
  苏打 / 白桦 / Mia / Chloe / Milo / Dean),`mimo-v2.5-tts-voicedesign` 按
  `prompt` 里的一句描述造音色(必填,`voice` 不用),`mimo-v2.5-tts-voiceclone`
  按 `sample_audio`(本机 wav/mp3,base64 后 ≤ 10MB)克隆。`style` 是加在文本开头
  的风格标签(`(温柔 慵懒)…`,情绪/语气/方言/角色都行;TUI 里是回车进多选菜单
  Tab 勾选,配置里逗号分隔,发请求时转成空格),`prompt` 是一句自然语言的
  提示词(TUI 里回车直接输入)。**没有语速/音量/音调数值参数**(官方文档 `audio`
  只有 format / voice),语速、语气、角色都写在提示词里,如「语速稍快,像在跟朋友
  聊天」,原样作为 user 消息发出。鉴权头
  `Authorization: Bearer` 与 `api-key` 都带(文档两种写法都有)。流式接口官方
  目前是"兼容模式"(推理完一次性回),所以走非流式。

**生效条件 = `voice.tts.enabled` 开 + 当前供应商的 key 非空**;不用再单独"激活"
(装上 miyu-voice、填 key、开开关三步即可)。TUI 里在某家填了 key 而当前那家没
key,会自动切过去;两家都有 key 时在「播报供应商」列表按 Tab 切换。合成好的 wav
由 daemon 交给 miyu-voice 播放;播报期间麦克风帧丢弃(没有回声消除,半双工),
`miyu listen` 会先掐掉播报再收听。

**通知与声音同步(09-05)**:此前「在听」通知为了不和「收到」连弹被压了 1.2s,
而提示音是即刻响的;回合完成时又是先弹通知再去合成(一到三秒)再播。现在:
Linux 上一串语音通知走 `notify-send -p/-r` 替换同一个气泡(「在听」→「收到」→
回复),「在听」与提示音同时出;回合完成先合成、合成好了通知和播放同一瞬间
发;播报输出流常开 30s 免去每次开设备的几十毫秒。macOS/Windows 没有替换
能力,「在听」仍延迟 1.2s。量尺 `testkit/voice/sync_timing.py`。

缺东西一律静默:没装 `miyu-voice` 只在 daemon 日志 warn 一次;没填 key 时开关
照样能开,填上的那一刻生效;QQ 语音转写不可用时留占位不报错。

两个独立开关:`voice.enabled` = **语音唤醒**(麦克风常开、唤醒词、听写、
`miyu listen`),`voice.tts.enabled` = **文本转语音**(回复播报、`speak` 工具)。
任一开启都会拉起 miyu-voice;唤醒关闭时它不开麦克风、不下载识别模型,只管播放。

TUI「语音功能」菜单:语音唤醒开关 → 文本转语音开关 → 「配置播报供应商」(列表里
`[*]` 是当前生效的,Enter 配置,Tab 设为当前;MiniMax:连接与模型 / **选择音色** /
播报参数(语速、音量、音调、情绪、试听语句)/ 试听;Xiaomi MiMo:连接、模型与音色 /
风格与指令 / 试听)→ 「识别与唤醒设置」。
MiniMax 选择音色的列表来自 `get_voice`(名字 + 描述,含克隆音色),`/` 进入过滤输入
(**边打边筛**,Esc 清空、Enter 保留过滤回到列表)、`t` 按标签
筛选(**多选**:Tab/空格勾 `[*]`,Enter 应用;语种之间取"或",女声/男声、克隆各成
一组,组间取"且",什么都不勾 = 全部)、`p` 试听当前行、`Enter` 选用;默认只勾「中文」。
列表行按终端宽度排:名字/描述优先,音色 id 放得下才带(`/` 搜索仍能匹配 id)。试听走 daemon(IPC `VoiceSpeak` 可携带
整份未保存的 tts 配置),默认句子「今天也是充满希望的一天」,可在播报参数里改。音调是半音偏移:
0 原声,正数更高更细,负数更低更沉,±12 一个八度。
`miyu voice say "文本"` / WebUI `POST /api/voice/tts/preview` 同样可试听;WebUI 设置页
按「播报供应商」下拉只显示当前那家的字段,MiniMax 音色列表走
`GET /api/voice/tts/voices?provider=minimax`。

**模型主动说话**:`speak` 工具(文本转语音激活时注册,只在本地会话的 normal
模式;QQ 会话通常是远程的,平台回合统一摘掉;dev 模式不给——提示词极简、没有
语音协议)把一句口语文本经播报供应商从扬声器播出。工具描述只说"这是说话的工具",什么时候用写在提示词里。

**从终端发到 QQ**:「接入通讯平台 → 允许 AI 从终端发消息到通讯平台」打开后,
本地会话(REPL / WebUI / shellhook)注册 `send_qq_message` 工具(平台会话不注册,
那边已有 `send_message_to_user`):`text` 必填,`voice: true` 发语音消息,`to` 的
可选项是管理员列表的别名(「允许使用终端的管理员 QQ 号」里每个号码可配别名,
没别名显示号码),不传发给第一个(主管理员)。normal 与 dev 模式都注册
(写代码时"跑完把结果发我手机"是真需求)。工具只在 NapCat 的反向 ws 已连上
时注册(连接状态并入回合资源的缓存键,连上/掉线各自重建一份工具表,也就是
这两个时刻本地会话的缓存前缀会变一次);掉线时模型根本看不到它。

**QQ 语音消息(出)**:`send_voice_message` 工具(平台会话、文本转语音激活时注册)
把文本合成后作为 OneBot `record` 段单独发一条(QQ 语音不能和文字混发),
NapCat 那边把 wav 转 silk。合成文本先过一遍清洗(去代码/链接/路径)。历史库里
记成 `[语音] 原文`(此前只有 `[语音]`,她自己说过什么别的会话都不知道)。
**发了语音就不再发正文**(09-06):语音发出后走与 `send_message_to_user` 直发同一条
抑制路,回合末尾模型再写的正文一律不发;另外模型"没话说"时爱吐一个零宽空格
(U+200B),`trim()` 不认它,之前会发出一条空气泡,现在不可见字符一律当空。

**QQ 语音消息(入)**(`onebot/voice_inbound.rs`,09-05):别人发的语音此前模型
完全看不见(只有一条 `[audio id=…]` 媒体记录,纯语音消息直接判"没有可见内容"
不回)。现在建 inbound_event 前把它转成文字接进正文:NapCat `get_record`
(要 wav)→ 交给 miyu-voice 转写 → `[语音] 文本`,模型、主动回复判官、历史库看
的都是这份。**要语音唤醒开着**(识别模型跟它加载)且前端已接上;没开、没装、
取不到文件、转写失败一律静默退化成 `[语音消息]` 占位,不弹通知不报错。

**快捷键呼叫**:`miyu listen` 是个**开关**:她没在听时让前端直接进入等待指令
状态(提示音 + 「在听」通知,8 秒内说指令),效果与喊唤醒词一样;已经在听(等
指令、追问窗口内、她正在回复/合成/播报)时再按一次就全停:关窗、掐掉回合、停掉
播报、**合成中还没播出的那段也作废**,通知气泡换成「不听了」(09-06;之前合成
那一两秒里按下去,窗口关了但音频照样播出来,看着像没生效)。绑到合成器快捷键上,
例如 niri:`Mod+Space { spawn "miyu" "listen"; }`。
成功时不输出;语音未启用或前端未就绪时报错退出;听写进行中不接管。

对话中说「没事了 / 就这样 / 去忙吧」→ 模型调 `end_voice_chat` 工具(仅
`voice.enabled` 时注册)→ 关窗。免唤醒追问窗口 `follow_up_seconds` 默认 **30**
(09-06 从 300 改小),**从她回复完起算**:回合运行、合成、播报期间计时都冻结
(`voice.hold` 压到合成结束才放,播报期间前端不喂帧),播完才开始数;每次回复都
重新起算。旧配置里存了 300 的要自己改。
文字照常落「语音会话」lane(独立 user lane,id 记在 `state/voice-session-id`),
WebUI 能翻实录;进上下文的只有识别文本,通知/提示音都在模型视野之外。

提示音五个(`assets/voice/{wake,heard,done,error,off}.wav`,木琴音色,24kHz mono,
`testkit/voice/sounds/synth.py` 生成,内嵌进 miyu-voice),`voice.sounds` 开关、
`voice.sound_volume` 音量,`miyu-voice cue done` 试听。

### 听写(谁有音频谁出音频)

| 入口 | 音频 | 文字去向 |
|---|---|---|
| REPL `/stt` | 本机麦(daemon 让前端开 10s 静默短窗;听写期间唤醒暂停) | 逐句填进编辑框;Esc 停止(文字保留),回车发送并停止;`dictation_auto_submit` 改直接提交 |
| `miyu stt` | 本机麦 | 第一句即提交为消息,前台流式打印回复(shellhook 形态) |
| WebUI 麦克风按钮 | 浏览器 `getUserMedia` → 前端重采样 16k PCM16 → WebSocket `/api/voice/stream` 持续推给 daemon → 转给 `miyu-voice`(VAD/分句/识别与本机麦同一套) | 识别一句回一句,逐句填进输入框;输入框上方悬浮麦克风电平指示;静默 10s 自动结束,再点麦克风或 Esc 也能结束 |

浏览器麦克风只在 https 或 localhost 可用;LAN http 页面按钮会提示改用 `/stt`。
`POST /api/voice/transcribe`(整段 WAV → 文本)保留给外部脚本,WebUI 不再用它。

听写认领是全局单例:REPL、`miyu stt`、WebUI 同一时刻只能有一个在听写。
浏览器流式听写期间 `miyu-voice` 丢弃本机麦克风的帧,结束后恢复唤醒监听。

## 六、配置(`voice` 节;TUI「语音功能」表单 / WebUI 设置页同名节)

```jsonc
"voice": {
  "enabled": false,
  "wake_keywords": ["未有未有", "密友密友", "miyumiyu", "みゆみゆ"],   // 可多个,任一命中即唤醒;写逗号分隔的字符串也行
  "wake_threshold": 0.25, "wake_boost": 1.0,
  "microphone": null,                 // TUI/WebUI 从 `miyu-voice devices` 列表里选;null=系统默认
  "stt_threads": 2,
  "stt_language": "zh",               // auto | zh | en | ja | ko | yue;auto 会把普通话判成日语
  "stt_unload_seconds": 60,           // 0 = 常驻
  "follow_up_seconds": 30,              // 从回复播完起算,每次回复重新起算
  "min_utterance_chars": 2,
  "sounds": true, "sound_volume": 0.6,
  "notify_reply_chars": 120,
  "dictation_auto_submit": false,
  "tts": {
    "active": "minimax",              // minimax | mimo;缺省即 minimax;生效 = enabled + 当前那家 api_key 非空
    "max_chars": 300,
    "minimax": {
      "api_key": "…",                 // 支持 "$env:MINIMAX_API_KEY"
      "base_url": "https://api.minimaxi.com/v1",   // 国际站 https://api.minimax.io/v1
      "model": "speech-2.6-turbo",
      "voice_id": "Chinese_sweet_girl_nv1",        // get_voice 列表里的 voice_id
      "speed": 1.0, "vol": 1.0, "pitch": 0, "emotion": "", "language_boost": "auto"
    },
    "mimo": {
      "api_key": "…",                 // 支持 "$env:MIMO_API_KEY"
      "base_url": "https://api.xiaomimimo.com/v1",
      "model": "mimo-v2.5-tts",       // | mimo-v2.5-tts-voicedesign | mimo-v2.5-tts-voiceclone
      "voice": "mimo_default",        // 冰糖 / 茉莉 / 苏打 / 白桦 / Mia / Chloe / Milo / Dean
      "style": "",                    // 文本开头的风格标签,如 "温柔,慵懒"
      "prompt": "",                   // user 消息:语速/语气/角色一句话,如「语速稍快,像在跟朋友聊天」;voicedesign 下是音色描述
      "sample_audio": null            // voiceclone 的参考音频路径(wav/mp3)
    }
  }
}
```

改动经 `miyu reload` / WebUI 保存生效:voice 节变了 daemon 就重启前端进程。

## 七、排查

- 状态:`GET /api/voice/status`(二进制在不在、是否 attached、采集设备、日志路径),
  IPC `VoiceStatus` 同源。
- 日志:`logs/voice-worker.log`(`MIYU_VOICE_DEBUG=1` 打各阶段耗时)。
- **"没反应"先跑 `miyu-voice test --keyword 未有未有 --timings`**:一行"听到语音"
  都没有 = 音频没进来(查 `miyu-voice devices` / 系统默认源);有但不命中 = 唤醒词
  层(试调低阈值 / 换声调错开的词)。
- 前端找不到:daemon 日志 warn「找不到 miyu-voice」;放到 miyu 同目录或 PATH。
- 麦克风占用冲突:同机只跑一个 miyu-voice;调试时 `MIYU_HOME` 隔离的 daemon
  也会拉起自己的一份。
- **提示音/播报全无、喊了没反应,但 `miyu-voice test` 单跑正常(09-06 事故)**:先看
  WirePlumber 有没有把 miyu-voice 的流"记住"到别的设备——
  `grep miyu-voice ~/.local/state/wireplumber/stream-properties`,任何带 `"target"`
  的条目都可疑(用 `pactl move-*`/pavucontrol 挪过一次流,WirePlumber 就按
  `application.name` 永久记住,之后每条 "PipeWire ALSA [miyu-voice]" 流都落到那
  里;测试夹具把播放流挪进 null sink 就是这么把线上提示音弄没的)。清法:
  `systemctl --user stop wireplumber` → 删掉那几行里的 `"target":...` → start →
  重启 miyu-voice(`miyu reload`)。再看 `pw-dump` 里 `alsa_capture.miyu-voice` 的
  Link 是不是接在你真正说话的那只麦上;两只全速 USB 声卡挂同一个 hub 时,一只被
  常开会让另一只 `dmesg: Not enough bandwidth for altsetting` 起不来,把配置
  `voice.microphone` 写成 `miyu-voice devices` 里那只麦的源名,别让它跟着"默认源"漂。

## 八、测试

- 单测:`cargo test --features voice --lib voice`(唤醒词剥离、能量门、WAV 编解码)、
  `cargo test --lib web::voice_bridge`(通知摘要)。
- 真机 e2e(需模型):`cargo test --features voice --lib voice::e2e -- --ignored`
  ——模型自带 test_wavs 走 VAD→KWS→STT。
- 唤醒词实验台:`MIYU_KW_WAVS=<wav目录> MIYU_KW_KEYWORDS=a,b MIYU_KW_EACH=1
  cargo test --features voice --lib voice::e2e_tests::keyword_lab -- --ignored --nocapture`
  输出 关键词×样本 命中矩阵(`MIYU_KW_STT=1` 附识别文本;`kws_raw` 绕开管线直接喂模型,
  用来区分"模型不认"与"切段问题");样本在 `testkit/voice/samples/`(MiniMax 合成)。
- 通知/音效/播报同步量尺:`testkit/voice/sync_timing.py --bin-dir <dir> --label x`
  (隔离 daemon + 真 MiniMax 播报引到 null sink + 假 notify-send 记时间戳 + pactl
  轮询播放流),输出 gap_wake / gap_reply 秒数。
- 全链隔离 e2e:`testkit/voice/e2e.py --bin-dir target/release`——隔离 daemon +
  桩 LLM + 假 notify-send + PipeWire null sink 注入,验唤醒→通知→回合→落库、
  听写认领、HTTP 转写、WebSocket 流式听写(标准库手写 ws 客户端推 PCM16)。
- 量尺:`testkit/voice/measure.py <bin> <label> --sub test --extra --timings [--cpus 0,1]`。

### PipeWire 夹具要点(踩过的坑)

- 播放注入用**普通 null sink**(`media.class=Audio/Source/Virtual` 播不进去);
- 采集端设 `PIPEWIRE_NODE=<sink>` + `PIPEWIRE_PROPS='{ stream.capture.sink = true }'`,
  WirePlumber 会把采集口接到 sink 的 monitor;不加后者它可能接到**真实麦克风**;
- 隔离 `XDG_RUNTIME_DIR` 时要设 `PIPEWIRE_RUNTIME_DIR` 指回真实运行目录;
- `node.autoconnect=false` 会让 cpal 打不开流。
- **绝不用 `pactl move-sink-input` / `move-source-output` 挪 miyu-voice 的流**:WirePlumber
  会把它记成该 application.name 的永久目标,污染用户线上(提示音进 null sink)。改接线
  用 `pw-link`(不会被记住,见 `sync_timing.py` 的 `relink_playback`);夹具加载的
  null sink 结束时 `pactl unload-module` 卸掉,别留在用户的声卡图里。

## 九、实测(release,16 核,2026-09-05)

v1(voice 分支)基线,与并行编译同跑有干扰,仅供量级参考:

| 相位 | CPU 均值 | RSS 高水位 | 备注 |
|---|---|---|---|
| 安静(VAD 逐帧) | 1.7% | 82MB | v2 加能量门后应接近 0 |
| KWS 判段(5s) | — | — | 冷 893ms,热 70~160ms |
| STT 首次加载+转写 | 峰 120% | 392MB | 加载 ≈2.1s |
| STT 热转写 | 峰 ~100% | 282MB 常驻 | 3~5s 音频 160~400ms |

v2(`miyu-voice test --timings`,同一夹具、同一批 test_wavs,机器同样有并行编译;
follow_up=300 使 STT 在整个测量期都不满足卸载条件):

| 相位 | 全核 CPU 均值/峰值 | 全核 RSS | 限 2 核 CPU 均值/峰值 | 限 2 核 RSS |
|---|---|---|---|---|
| 安静(能量门 + VAD) | **0.4%** / 2% | 79MB 稳 | 0.6% / 4% | 74MB(高水位 73MB) |
| 有人声不命中(KWS) | 9.3% / 82% | →397MB(STT 命中后加载) | 6.7% / 90% | →379MB |
| 命中 → STT | 5.9% / 68% | 高水位 415MB | 6.3% / 68% | 高水位 395MB |
| STT 热转写 | 9.7% / 80% | 242→307MB | 4.8% / 66% | 279→281MB |
| 之后安静(窗口未关,STT 未卸) | 1.4% / 4% | 307MB | 0.7% / 2% | 231MB |

| 阶段 | 全核中位 / 最大 | 限 2 核中位 / 最大 | 每秒音频(中位) |
|---|---|---|---|
| KWS 判段 | 74ms / 109ms | 62ms / 821ms | 17~24ms/s |
| STT 首次加载 | 3192ms | 1432ms | — |
| STT 热转写 | 152ms / 7106ms* | 168ms / 2288ms* | 45~58ms/s |

\* 最大值出现在别的会话并行 release 编译时,同一段音频重放为 150~250ms;
空载机器上的单测里 STT 加载 875ms、3.5s 音频转写 133ms(debug 构建)。

结论:安静态 CPU 从 1.7% 降到 0.4%,唤醒态常驻 ≈ 75~80MB;识别是 CPU 短脉冲
(2 核与 16 核几乎一样快),弱机的真正代价是 STT 常驻的 ~300MB,由
`stt_unload_seconds` 兜底;二进制:`miyu` 58.1MB(与无语音版持平),`miyu-voice` 35.9MB。

隔离全链 e2e(`testkit/voice/e2e.py`,桩 LLM)09-05 通过:attach、唤醒→「收到」→
桩回复→完成通知(含摘要)→语音会话落 1 轮、听写认领拿到文本、HTTP 转写、前端随
daemon 退出。
