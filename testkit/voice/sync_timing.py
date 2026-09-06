#!/usr/bin/env python3
"""语音"通知 vs 音效/播报"同步量尺(隔离环境,不碰线上 daemon、不碰真实麦克风)。

夹具与 e2e.py 同源:隔离 MIYU_HOME + XDG_RUNTIME_DIR、桩 LLM、PipeWire null sink
注入音频、假 notify-send(记 epoch 时间戳)。播报走真实 MiniMax(从本机
config.jsonc 拿 voice.tts 配置),播放被 PIPEWIRE_NODE 引到同一个 null sink,
用户扬声器听不到;播放流再用 pw-link 直连到第二个 null sink(见 relink_playback,
千万别用 pactl move-sink-input)。

量什么(时间都相对播放开始):
  cue_stream     唤醒提示音真正响起(第二个 null sink 的 monitor 上测到声音)
  notify_listen  「未有在听」通知
  notify_heard   「未有收到」通知
  notify_reply   回合完成通知「未有 …」
  tts_stream     播报声音真正响起(持续 ≥0.8s 的有声区间起点)
两个差值就是用户感受到的"不同步":
  gap_wake  = notify_listen - cue_stream   (老版本按设计压了 1.2s)
  gap_reply = tts_stream  - notify_reply   (老版本先弹通知再去合成)

用法: testkit/voice/sync_timing.py --bin-dir target/release [--label new]
      两个 --bin-dir 各跑一次,对比表格。
"""
import argparse, json, os, shutil, socket, subprocess, sys, threading, time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "testkit" / "persona-ab"))
from run import strip_jsonc  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--bin-dir", default=str(REPO / "target" / "release"))
ap.add_argument("--label", default="")
ap.add_argument("--wake-wav", default=str(Path(__file__).parent / "samples" / "weiyou.wav"),
                help="只含唤醒词的 16k 音频(量唤醒通知);缺省用仓库样本")
ap.add_argument("--rounds", type=int, default=2)
args = ap.parse_args()
BIN_DIR = Path(args.bin_dir).resolve()
MIYU, VOICE = BIN_DIR / "miyu", BIN_DIR / "miyu-voice"
assert MIYU.is_file() and VOICE.is_file(), f"缺二进制: {MIYU} / {VOICE}"

WORK = Path("/tmp/miyu-voice-sync")
HOME, RUN_DIR, FAKEBIN = WORK / "home", WORK / "run", WORK / "bin"
NOTIFY_LOG, STUB_LOG = WORK / "notify.log", WORK / "stub.jsonl"
STUB_PORT, WEB_PORT = 18495, 18494
SINK = "miyu_sync_sink"
MODELS = Path(os.environ.get("MIYU_VOICE_MODELS_DIR", Path.home() / ".miyu/state/models"))
KWS = MODELS / "sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01/test_wavs"
REAL_CONFIG = Path.home() / ".miyu/config/config.jsonc"
t0 = time.time()
def say(msg): print(f"{time.time()-t0:7.2f}s {msg}", flush=True)

if WORK.exists(): shutil.rmtree(WORK)
for d in (HOME / "config", HOME / "state", RUN_DIR, FAKEBIN): d.mkdir(parents=True)
(HOME / "state" / "models").symlink_to(MODELS)
# 假 notify-send:记 epoch 秒 + 参数;新版会带 -p 要 id,这里不打印 id(等于老版 notify-send)。
(FAKEBIN / "notify-send").write_text(
    '#!/bin/sh\nprintf "%s\\t" "$(date +%s.%N)" "$@" >> "$NOTIFY_LOG"; printf "\\n" >> "$NOTIFY_LOG"\n')
(FAKEBIN / "notify-send").chmod(0o755)
real = json.loads(strip_jsonc(REAL_CONFIG.read_text()), strict=False)
tts = real.get("voice", {}).get("tts", {})
assert tts.get("minimax", {}).get("api_key"), "本机 config 里没有 voice.tts.minimax.api_key,量不了播报"
cfg = dict(real)
for key in ("platforms", "web", "alarm", "mcp"): cfg.pop(key, None)
cfg["providers"] = [{"enabled": True, "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
                     "protocol": "openai-chat", "api_key": "stub-key", "models": ["stub-model"]}]
cfg["active_provider"] = "stub"
cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-model"}]
cfg.pop("active_multimodal_provider_models", None)
cfg.setdefault("memory", {})["enabled"] = False
cfg.setdefault("cache", {})["request_log"] = False
cfg.setdefault("notifications", {})["enabled"] = True
cfg["voice"] = {"enabled": True, "wake_keywords": ["周望军", "未有未有"], "follow_up_seconds": 0,
                "stt_unload_seconds": 300, "sounds": True, "sound_volume": 0.3, "notify_reply_chars": 60,
                "tts": {**tts, "enabled": True, "active": "minimax", "max_chars": 80}}
(HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2))

mods = subprocess.run(["pactl", "list", "short", "modules"], capture_output=True, text=True).stdout
loaded_modules = []  # 本脚本加载的 null sink 模块 id,结束时卸掉(留着会污染用户的 PipeWire 图)
def load_null_sink(name):
    r = subprocess.run(["pactl", "load-module", "module-null-sink", f"sink_name={name}", "channel_map=front-left,front-right"], check=True, capture_output=True, text=True)
    loaded_modules.append(r.stdout.strip())
if SINK not in mods:
    load_null_sink(SINK)

env = dict(os.environ)
for k in ("XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "MIYU_DIRECT"): env.pop(k, None)
env.update(MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUN_DIR), NOTIFY_LOG=str(NOTIFY_LOG), LANG="zh_CN.UTF-8",
           PATH=f"{FAKEBIN}:{env.get('PATH', '')}",
           PIPEWIRE_RUNTIME_DIR=os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"),
           PIPEWIRE_NODE=SINK, PIPEWIRE_PROPS="{ stream.capture.sink = true }", MIYU_VOICE_DEBUG="1")

stub = subprocess.Popen([sys.executable, str(REPO / "testkit/low-footprint/stub_llm.py")],
                        env={**os.environ, "STUB_PORT": str(STUB_PORT), "STUB_LOG": str(STUB_LOG), "STUB_CHUNKS": "8", "STUB_DELAY_MS": "50"},
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
daemon_log = open(WORK / "daemon.log", "w")
daemon = subprocess.Popen([str(MIYU), "__daemon", "--port", str(WEB_PORT), "--bind", "127.0.0.1"], env=env,
                          stdout=daemon_log, stderr=subprocess.STDOUT, preexec_fn=os.setsid)

# ---------- 播放探针 ----------
# miyu-voice 的提示音流和播报流在 pactl 里长得一样(都是 "PipeWire ALSA [miyu-voice]"),
# 光看流出现的时刻分不清谁是谁(提示音流会常开 30s)。做法:新出现的 miyu-voice 流
# 立刻挪到第二个 null sink(OUT),parec 录 OUT 的 monitor,按能量判"什么时候真的
# 有声音":短促(<0.6s)的是提示音,持续的是播报。注入的测试音频在 SINK,不会串进来。
OUT = "miyu_sync_out"
if OUT not in mods:
    load_null_sink(OUT)

def relink_playback(node_id):
    """把 miyu-voice 新出现的播放流用 pw-link 直连到 OUT。

    **不能用 `pactl move-sink-input`**:WirePlumber 会把 move 当成用户意愿记进
    ~/.local/state/wireplumber/stream-properties(按 application.name 记),此后每个
    "PipeWire ALSA [miyu-voice]" 播放流——线上的提示音和播报——都永久落进这个 null
    sink,用户就此"提示音消失、没有播报"(09-06 事故)。pw-link 直连不会被记住。"""
    nid = int(node_id)
    for _ in range(5):
        dump = json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True).stdout)
        sink_id = next((o["id"] for o in dump if o.get("type") == "PipeWire:Interface:Node" and o["info"]["props"].get("node.name") == OUT), None)
        my_ports, out_ports = {}, {}
        for o in dump:
            if o.get("type") != "PipeWire:Interface:Port": continue
            props = o["info"]["props"]
            channel = props.get("audio.channel", props.get("port.name"))
            if props.get("node.id") == nid and props.get("port.direction") == "out": my_ports[channel] = o["id"]
            if props.get("node.id") == sink_id and props.get("port.direction") == "in": out_ports[channel] = o["id"]
        if my_ports and out_ports: break
        time.sleep(0.05)
    else:
        say(f"!! 播放流 {node_id} 端口没就位,没接到 {OUT}"); return
    for o in dump:
        if o.get("type") == "PipeWire:Interface:Link" and o["info"].get("output-node-id") == nid:
            subprocess.run(["pw-link", "-d", str(o["id"])], capture_output=True)
    for channel, port in my_ports.items():
        dst = out_ports.get(channel) or next(iter(out_ports.values()))
        subprocess.run(["pw-link", str(port), str(dst)], capture_output=True)

def check_wp_pins():
    """回归哨兵:夹具跑完后 WirePlumber 不该记住任何 miyu-voice 流的固定目标。"""
    state = Path.home() / ".local/state/wireplumber/stream-properties"
    if not state.exists(): return
    for line in state.read_text().splitlines():
        if "miyu-voice" in line and '"target"' in line:
            say(f"!! WirePlumber 记住了 miyu-voice 流的固定目标,线上会失声,请手动清掉: {line[:140]}")
streams = []  # (epoch, id)
seen = set()
stop_probe = threading.Event()
def probe():
    while not stop_probe.is_set():
        out = subprocess.run(["pactl", "list", "short", "sink-inputs"], capture_output=True, text=True).stdout
        now = time.time()
        for line in out.splitlines():
            sid = line.split("\t")[0]
            if sid in seen: continue
            seen.add(sid)
            detail = subprocess.run(["pactl", "list", "sink-inputs"], capture_output=True, text=True).stdout
            block = next((part for part in detail.split("Sink Input #") if part.startswith(sid + "\n")), "")
            if "miyu-voice" in block:
                relink_playback(sid)  # pactl 的 sink-input id 就是 PipeWire 节点 id
                streams.append((now, sid))
        time.sleep(0.01)
threading.Thread(target=probe, daemon=True).start()

# 能量探针:parec 16k mono s16 → 每 10ms 一个 RMS 样本。parec 成批交付,同批样本的
# 到达时间一样,所以时间不用到达时刻,而按"首个样本到达时刻 + 序号×10ms"推算
# (null sink 的 monitor 是连续流,静音也出样本,序号即时钟)。
levels = []       # rms 序列
rec_start = [None]
parec = subprocess.Popen(["parec", "-d", f"{OUT}.monitor", "--format=s16le", "--rate=16000", "--channels=1", "--latency-msec=10"],
                         stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
def level_reader():
    import array
    frame = 320  # 10ms
    while not stop_probe.is_set():
        data = parec.stdout.read(frame)
        if not data: break
        if rec_start[0] is None:
            rec_start[0] = time.time() - 0.01
        samples = array.array("h", data)
        levels.append((sum(x * x for x in samples) / max(1, len(samples))) ** 0.5 / 32768.0)
threading.Thread(target=level_reader, daemon=True).start()

def sample_time(index):
    return (rec_start[0] or 0) + index * 0.01

def first_burst(after, min_len, threshold=0.01, before=None):
    """after 之后第一个"接下来 min_len 秒里至少一半样本有声"的有声样本的时刻。
    语音在音节间会有几十毫秒的低谷,不能要求逐样本连续有声。"""
    need = max(1, int(min_len / 0.01))
    snapshot = list(levels)
    for index, rms in enumerate(snapshot):
        stamp = sample_time(index)
        if stamp < after or rms < threshold: continue
        if before is not None and stamp > before: break
        window = snapshot[index:index + need]
        if len(window) < need: break
        if sum(1 for r in window if r >= threshold) * 2 >= need:
            return stamp
    return None

def http(method, path, body=None):
    import urllib.request
    req = urllib.request.Request(f"http://127.0.0.1:{WEB_PORT}{path}", data=body, method=method)
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.loads(resp.read())

def wait_for(pred, timeout, what):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if pred(): return True
        except Exception:
            pass
        time.sleep(0.05)
    say(f"!! 超时: {what}"); return False

def notifications():
    """[(epoch, title, body)];假 notify-send 行尾多一个制表符,先去掉。"""
    rows = []
    if NOTIFY_LOG.exists():
        for line in NOTIFY_LOG.read_text().splitlines():
            parts = line.rstrip("\t").split("\t")
            if len(parts) < 3: continue
            rows.append((float(parts[0]), parts[-2], parts[-1]))
    return rows

def notify_time(after, title):
    for stamp, got, _ in notifications():
        if stamp >= after and got == title:
            return stamp
    return None

def socket_path():
    found = sorted(RUN_DIR.glob("miyu*/core.sock"))
    return found[0] if found else None

def play(path):
    subprocess.run(["pw-play", "--target", SINK, str(path)], check=True)

rows = []
try:
    assert wait_for(lambda: socket_path() is not None, 20, "daemon socket") and daemon.poll() is None
    assert wait_for(lambda: http("GET", "/api/voice/status").get("attached") is True, 60, "miyu-voice attach")
    time.sleep(2)
    # 预热:第一次唤醒常被能量门/噪声底自适应吃掉,先喊一次不计数。
    play(Path(args.wake_wav)); time.sleep(10)
    for round_index in range(args.rounds):
        # A. 只喊唤醒词 → 提示音流 vs「在听」通知
        wake_wav = Path(args.wake_wav)
        started = time.time(); play(wake_wav)
        wait_for(lambda: notify_time(started, "未有在听") is not None, 15, "「在听」通知")
        time.sleep(0.8)
        cue = first_burst(started, 0.08)
        listen = notify_time(started, "未有在听")
        rows.append({"round": round_index, "phase": "wake", "cue_stream": cue and cue - started,
                     "notify_listen": listen and listen - started,
                     "gap_wake": (listen - cue) if (cue and listen) else None})
        # 等唤醒窗口超时回到待唤醒(AWAIT 8s)
        time.sleep(9.5)
        # B. 唤醒词+指令 → 桩回复 → 通知「未有」 vs 播报流
        started = time.time(); play(KWS / "5.wav")
        wait_for(lambda: notify_time(started, "未有收到") is not None, 20, "「收到」通知")
        heard = notify_time(started, "未有收到")
        wait_for(lambda: notify_time(started, "未有") is not None, 40, "回复通知")
        reply = notify_time(started, "未有")
        # 播报 = 「收到」之后第一段持续 ≥0.8s 的声音(提示音只有零点几秒)。
        wait_for(lambda: first_burst((heard or started) + 0.6, 0.8) is not None, 40, "播报声音")
        time.sleep(0.5)
        tts_stream = first_burst((heard or started) + 0.6, 0.8)
        rows.append({"round": round_index, "phase": "reply", "notify_heard": heard and heard - started,
                     "notify_reply": reply and reply - started, "tts_stream": tts_stream and tts_stream - started,
                     "gap_reply": (tts_stream - reply) if (tts_stream and reply) else None})
        # 等播报完 + 窗口回落
        time.sleep(12)
finally:
    stop_probe.set()
    try: parec.terminate()
    except Exception: pass
    try:
        sock = socket.socket(socket.AF_UNIX); sock.settimeout(3)
        sock.connect(str(socket_path()))
        payload = json.dumps({"version": 3, "command": "shutdown"}).encode()
        sock.sendall(len(payload).to_bytes(4, "big") + payload); sock.close()
    except Exception:
        pass
    time.sleep(1)
    if daemon.poll() is None:
        daemon.terminate()
        try: daemon.wait(5)
        except subprocess.TimeoutExpired: daemon.kill()
    stub.terminate()
    for pid in subprocess.run(["pgrep", "-f", f"{VOICE}$"], capture_output=True, text=True).stdout.split():
        subprocess.run(["kill", pid])
    for module in loaded_modules:
        subprocess.run(["pactl", "unload-module", module], capture_output=True)
    check_wp_pins()
    label = args.label or BIN_DIR.name
    print(f"\n== {label} ({BIN_DIR})")
    print(f"  探针: miyu-voice 流 {len(streams)} 条 {[round(s[0]-t0, 2) for s in streams]};能量样本 {len(levels)} 个,峰值 {max(levels, default=0):.3f},有声样本 {sum(1 for r in levels if r >= 0.01)}")
    for row in rows:
        print("  " + "  ".join(f"{k}={v:.3f}" if isinstance(v, float) else f"{k}={v}" for k, v in row.items()))
    (WORK / f"result-{label}.json").write_text(json.dumps(rows, ensure_ascii=False, indent=2))
    (WORK / f"levels-{label}.json").write_text(json.dumps({"rec_start": rec_start[0], "t0": t0, "levels": [round(r, 4) for r in levels]}))
    print(f"  通知: {NOTIFY_LOG}  日志: {WORK}/daemon.log  {HOME}/state/logs/voice-worker.log")
