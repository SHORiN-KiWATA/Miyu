# Miyu 语音功能(v2):唤醒、识别、听写

> 2026-09-05 重做。v1(08-14,voice 分支)只当参考件,设计取舍见
> `docs/plan/2026-09-05-voice-v2.md`。本文是现状:怎么装、怎么用、怎么排查、
> 各部件住在哪、实测数字。**不做 TTS**(语音回复走桌面通知 + 提示音)。

## 一、一句话

装上可选组件 `miyu-voice`,设置里开「语音功能」,daemon 会拉起一个独立进程
常开麦克风:喊「未有未有」→ 提示音 + 通知「未有在听」→ 说指令 → 通知「未有
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

唤醒词任意汉字:每个字的带调全拼按 KWS `tokens.txt` 最长匹配拆分(`keywords.rs`),
不用重训。`wake_threshold`(默认 0.25,越低越灵敏)/ `wake_boost`(默认 1.0,越大越灵敏)
直接对应 sherpa 的 keywords_threshold / keywords_score。

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
- 窗口计时冻结:回合运行期间(`voice.hold`)静默不消耗追问窗口。
- 打断:窗口内持续 0.3s 人声 → `speech_start` → daemon 取消进行中的回合。

## 五、交互

| 事件 | 提示音 | 桌面通知 |
|---|---|---|
| 唤醒词命中 | wake(上行两音) | 「未有在听 / 请讲」 |
| 识别出指令 | heard(单点) | 「未有收到 / <指令>」 |
| 回合完成 | done(下行三音) | 「未有 / <回复前 N 字>」(N=`notify_reply_chars`) |
| 回合失败 | error(低音) | 「语音会话出错 / …」 |
| 窗口关闭/超时 | 无 | 无 |

**语音会话**:唤醒对话落在一条专属会话(kind = `voice`,id 记在
`state/voice-session-id`),不进 WebUI 列表;用 `miyu voice history [--limit n]`
回看、`miyu voice reset` 清空(下次唤醒重建)、`miyu voice status` 看前端状态。

**回复播报(TTS)**:播报供应商独立于 LLM 的 providers 配置(免得混),目前预置
MiniMax:`voice.tts.minimax` 里填自己的 api_key(账号级,对话与语音共用一把,
支持 `$env:VAR`)、国内/国际站地址、模型、音色、语速/音量/音调/情绪/语种增强;
`voice.tts.active = "minimax"` 即激活,空则只弹通知和提示音。合成走 `t2a_v2`
返回 wav,由 daemon 交给 miyu-voice 播放;播报期间麦克风帧丢弃(没有回声消除,
半双工),`miyu listen` 会先掐掉播报再收听。

两个独立开关:`voice.enabled` = **语音唤醒**(麦克风常开、唤醒词、听写、
`miyu listen`),`voice.tts.enabled` = **文本转语音**(回复播报、`speak` 工具)。
任一开启都会拉起 miyu-voice;唤醒关闭时它不开麦克风、不下载识别模型,只管播放。

TUI「语音功能」菜单:语音唤醒开关 → 文本转语音开关 → 「配置播报供应商」(列表里 Tab 激活、Enter 进
MiniMax:连接与模型 / **选择音色** / 播报参数(语速、音量、音调、情绪、试听语句)/ 试听)→ 「识别与唤醒设置」。
选择音色的列表来自 `get_voice`(名字 + 描述,含克隆音色),`/` 搜索、`t` 按标签
(语种 / 女声 / 男声 / 克隆,标签由 id 与描述推得)筛选、`p` 试听当前行、
`Enter` 选用;默认只显示「中文」标签。试听走 daemon(IPC `VoiceSpeak` 可携带
整份未保存的 tts 配置),默认句子「今天也是充满希望的一天」,可在播报参数里改。音调是半音偏移:
0 原声,正数更高更细,负数更低更沉,±12 一个八度。
`miyu voice say "文本"` / WebUI `POST /api/voice/tts/preview` 同样可试听。

**模型主动说话**:`speak` 工具(文本转语音激活时注册,只在本地会话;QQ 会话
通常是远程的,平台回合统一摘掉)把一句口语文本经播报供应商从扬声器播出。工具描述只说"这是说话的工具",什么时候用写在提示词里。

**从终端发到 QQ**:「接入通讯平台 → 允许 AI 从终端发消息到通讯平台」打开后,
本地会话(REPL / WebUI / shellhook)注册 `send_qq_message` 工具(平台会话不注册,
那边已有 `send_message_to_user`):`text` 必填,`voice: true` 发语音消息,`to` 的
可选项是管理员列表的别名(「允许使用终端的管理员 QQ 号」里每个号码可配别名,
没别名显示号码),不传发给第一个(主管理员)。工具只在 NapCat 的反向 ws 已连上
时注册(连接状态并入回合资源的缓存键,连上/掉线各自重建一份工具表,也就是
这两个时刻本地会话的缓存前缀会变一次);掉线时模型根本看不到它。

**QQ 语音消息**:`send_voice_message` 工具(平台会话、文本转语音激活时注册)
把文本合成后作为 OneBot `record` 段单独发一条(QQ 语音不能和文字混发),
NapCat 那边把 wav 转 silk。合成文本先过一遍清洗(去代码/链接/路径)。

**快捷键呼叫**:`miyu listen` 让前端直接进入等待指令状态(提示音 + 「在听」
通知,8 秒内说指令),效果与喊唤醒词一样。绑到合成器快捷键上,例如 niri:
`Mod+Space { spawn "miyu" "listen"; }`。成功时不输出;语音未启用或前端未就绪
时报错退出。

对话中说「没事了 / 就这样 / 去忙吧」→ 模型调 `end_voice_chat` 工具(仅
`voice.enabled` 时注册)→ 关窗。免唤醒追问窗口 `follow_up_seconds` 默认 300。
文字照常落「语音会话」lane(独立 user lane,id 记在 `state/voice-session-id`),
WebUI 能翻实录;进上下文的只有识别文本,通知/提示音都在模型视野之外。

提示音四个(`assets/voice/{wake,heard,done,error}.wav`,木琴音色,24kHz mono,
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
  "wake_keywords": ["未有未有"],   // 可多个,任一命中即唤醒;写逗号分隔的字符串也行
  "wake_threshold": 0.25, "wake_boost": 1.0,
  "microphone": null,                 // TUI/WebUI 从 `miyu-voice devices` 列表里选;null=系统默认
  "stt_threads": 2,
  "stt_language": "zh",               // auto | zh | en | ja | ko | yue;auto 会把普通话判成日语
  "stt_unload_seconds": 60,           // 0 = 常驻
  "follow_up_seconds": 300,
  "min_utterance_chars": 2,
  "sounds": true, "sound_volume": 0.6,
  "notify_reply_chars": 120,
  "dictation_auto_submit": false,
  "tts": {
    "active": "minimax",              // minimax | 缺省 = 不播报
    "max_chars": 300,
    "minimax": {
      "api_key": "…",                 // 支持 "$env:MINIMAX_API_KEY"
      "base_url": "https://api.minimaxi.com/v1",   // 国际站 https://api.minimax.io/v1
      "model": "speech-2.6-turbo",
      "voice_id": "Chinese_sweet_girl_nv1",        // get_voice 列表里的 voice_id
      "speed": 1.0, "vol": 1.0, "pitch": 0, "emotion": "", "language_boost": "auto"
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

## 八、测试

- 单测:`cargo test --features voice --lib voice`(唤醒词剥离、能量门、WAV 编解码)、
  `cargo test --lib web::voice_bridge`(通知摘要)。
- 真机 e2e(需模型):`cargo test --features voice --lib voice::e2e -- --ignored`
  ——模型自带 test_wavs 走 VAD→KWS→STT。
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
