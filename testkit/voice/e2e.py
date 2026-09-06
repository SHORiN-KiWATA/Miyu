#!/usr/bin/env python3
"""语音 v2 全链 e2e(隔离环境,不碰线上 daemon、不碰真实麦克风):

  隔离 MIYU_HOME + 隔离 XDG_RUNTIME_DIR(IPC socket 不与 8300 撞)
  桩 LLM(OpenAI 兼容 SSE)← daemon(__daemon --port) → miyu-voice(同目录)
  PipeWire null sink 注入模型自带 test_wavs;stream.capture.sink 让采集口
  自动接到 sink 的 monitor(不会接到真实麦克风)
  假 notify-send 前置到 PATH,记录每条桌面通知

验证点:
  1. daemon 拉起 miyu-voice 并 attach(voice.status attached=true)
  2. 播放含唤醒词的音频 → 通知「未有收到」(同一口气带指令时「在听」被抑制)
  3. 桩 LLM 收到一次请求;完成后通知「未有」+ 回复摘要
  4. 「语音会话」lane 里落了一轮(user + assistant)
  5. StartDictation 认领 → 播放一段音频 → 收到 voice.dictation 文本
  6. /api/voice/transcribe 上传 wav → 返回文本
  7. WebSocket /api/voice/stream 推 16k PCM16 → 逐句收到 dictation 文本

用法: testkit/voice/e2e.py [--bin-dir target/release]
需要: ~/.miyu/state/models 已有模型(或 MIYU_VOICE_MODELS_DIR)。
"""
import argparse, base64, json, os, shutil, socket, subprocess, sys, time, threading
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "testkit" / "persona-ab"))
from run import strip_jsonc  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--bin-dir", default=str(REPO / "target" / "release"))
ap.add_argument("--keep", action="store_true", help="结束后保留隔离目录")
args = ap.parse_args()
BIN_DIR = Path(args.bin_dir).resolve()
MIYU = BIN_DIR / "miyu"
VOICE = BIN_DIR / "miyu-voice"
assert MIYU.is_file() and VOICE.is_file(), f"缺二进制: {MIYU} / {VOICE}"

WORK = Path("/tmp/miyu-voice-e2e")
HOME = WORK / "home"
RUN_DIR = WORK / "run"
FAKEBIN = WORK / "bin"
NOTIFY_LOG = WORK / "notify.log"
STUB_LOG = WORK / "stub.jsonl"
STUB_PORT, WEB_PORT = 18493, 18492
SINK = "miyu_test_sink"
KWS = Path(os.environ.get("MIYU_VOICE_MODELS_DIR", Path.home() / ".miyu/state/models")) / \
    "sherpa-onnx-kws-zipformer-wenetspeech-3.3M-2024-01-01/test_wavs"
REAL_CONFIG = Path.home() / ".miyu/config/config.jsonc"
t0 = time.time()
def say(msg): print(f"{time.time()-t0:7.2f}s {msg}", flush=True)

# ---------- 夹具 ----------
if WORK.exists(): shutil.rmtree(WORK)
for d in (HOME / "config", HOME / "state", RUN_DIR, FAKEBIN): d.mkdir(parents=True)
(HOME / "state" / "models").symlink_to(KWS.parent.parent)
(FAKEBIN / "notify-send").write_text('#!/bin/sh\nprintf "%s\\t" "$@" >> "$NOTIFY_LOG"; printf "\\n" >> "$NOTIFY_LOG"\n')
(FAKEBIN / "notify-send").chmod(0o755)
cfg = json.loads(strip_jsonc(REAL_CONFIG.read_text()), strict=False) if REAL_CONFIG.exists() else {"config_version": 1}
for key in ("platforms", "web", "alarm", "mcp"): cfg.pop(key, None)
cfg["providers"] = [{"enabled": True, "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
                     "protocol": "openai-chat", "api_key": "stub-key", "models": ["stub-model"]}]
cfg["active_provider"] = "stub"
cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-model"}]
cfg.pop("active_multimodal_provider_models", None)
cfg.setdefault("memory", {})["enabled"] = False
cfg.setdefault("cache", {})["request_log"] = False
cfg["voice"] = {"enabled": True, "wake_keyword": "周望军", "follow_up_seconds": 0, "stt_unload_seconds": 5,
                "sounds": False, "notify_reply_chars": 60}
(HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2))

mods = subprocess.run(["pactl", "list", "short", "modules"], capture_output=True, text=True).stdout
loaded_modules = []  # 本脚本加载的 null sink,结束时卸掉(留着会污染用户的 PipeWire 图)
if SINK not in mods:
    r = subprocess.run(["pactl", "load-module", "module-null-sink", f"sink_name={SINK}", "channel_map=front-left,front-right"], check=True, capture_output=True, text=True)
    loaded_modules.append(r.stdout.strip())

env = dict(os.environ)
for k in ("XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "MIYU_DIRECT"): env.pop(k, None)
env.update(MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUN_DIR), NOTIFY_LOG=str(NOTIFY_LOG), LANG="zh_CN.UTF-8",
           PATH=f"{FAKEBIN}:{env.get('PATH', '')}",
           # XDG_RUNTIME_DIR 被隔离后 PipeWire 找不到 socket,显式指回真实目录。
           PIPEWIRE_RUNTIME_DIR=os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"),
           PIPEWIRE_NODE=SINK, PIPEWIRE_PROPS="{ stream.capture.sink = true }", MIYU_VOICE_DEBUG="1")

stub = subprocess.Popen([sys.executable, str(REPO / "testkit/low-footprint/stub_llm.py")],
                        env={**os.environ, "STUB_PORT": str(STUB_PORT), "STUB_LOG": str(STUB_LOG), "STUB_CHUNKS": "8", "STUB_DELAY_MS": "50"},
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
daemon_log = open(WORK / "daemon.log", "w")
daemon = subprocess.Popen([str(MIYU), "__daemon", "--port", str(WEB_PORT), "--bind", "127.0.0.1"], env=env, stdout=daemon_log, stderr=subprocess.STDOUT, preexec_fn=os.setsid)
results = {}

def socket_path():
    """MIYU_HOME 隔离时运行目录叫 miyu-<hash>,用 glob 找。"""
    found = sorted(RUN_DIR.glob("miyu*/core.sock"))
    return found[0] if found else None

def ipc(command, read_events=0, timeout=10.0):
    """发一条 IPC 命令,返回收到的帧列表(长度前缀 JSON 帧格式见 ipc/protocol.rs)。"""
    sock = socket.socket(socket.AF_UNIX); sock.settimeout(timeout)
    sock.connect(str(socket_path()))
    payload = json.dumps({"version": 3, **command}).encode()
    sock.sendall(len(payload).to_bytes(4, "big") + payload)
    frames = []
    try:
        while len(frames) < 1 + read_events:
            head = sock.recv(4)
            if len(head) < 4: break
            size = int.from_bytes(head, "big"); body = b""
            while len(body) < size:
                chunk = sock.recv(size - len(body))
                if not chunk: break
                body += chunk
            frames.append(json.loads(body))
    except socket.timeout:
        pass
    return sock, frames

def wait_for(pred, timeout, what):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if pred(): return True
        except Exception:
            pass
        time.sleep(0.5)
    say(f"!! 超时: {what}"); return False

def play(name):
    say(f"play {name}"); subprocess.run(["pw-play", "--target", SINK, str(KWS / name)], check=True)

def notifications():
    return NOTIFY_LOG.read_text().splitlines() if NOTIFY_LOG.exists() else []

# 最小 WebSocket 客户端(标准库,客户端帧必须掩码)。
def ws_connect(path, timeout=30):
    sock = socket.create_connection(("127.0.0.1", WEB_PORT), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall((f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{WEB_PORT}\r\nUpgrade: websocket\r\n"
                  f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = sock.recv(4096)
        if not chunk: raise EOFError("ws handshake: connection closed")
        head += chunk
    status = head.split(b"\r\n", 1)[0]
    assert b" 101 " in status, f"ws handshake failed: {status!r}"
    return sock

def ws_send(sock, payload, opcode):
    mask = os.urandom(4); n = len(payload)
    head = bytes([0x80 | opcode])
    if n < 126: head += bytes([0x80 | n])
    elif n < 65536: head += bytes([0x80 | 126]) + n.to_bytes(2, "big")
    else: head += bytes([0x80 | 127]) + n.to_bytes(8, "big")
    sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

def ws_recv(sock):
    def read(n):
        buf = b""
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk: raise EOFError("ws closed")
            buf += chunk
        return buf
    b0, b1 = read(2)
    opcode, n = b0 & 0x0F, b1 & 0x7F
    if n == 126: n = int.from_bytes(read(2), "big")
    elif n == 127: n = int.from_bytes(read(8), "big")
    if b1 & 0x80: read(4)
    return opcode, read(n)

def http(method, path, body=None, ctype="application/json"):
    import urllib.request
    req = urllib.request.Request(f"http://127.0.0.1:{WEB_PORT}{path}", data=body, method=method)
    if body is not None: req.add_header("Content-Type", ctype)
    with urllib.request.urlopen(req, timeout=90) as resp:
        return json.loads(resp.read())

try:
    ok = wait_for(lambda: socket_path() is not None, 20, "daemon socket")
    assert ok and daemon.poll() is None, "daemon 没起来"
    # 1. worker attach
    ok = wait_for(lambda: http("GET", "/api/voice/status").get("attached") is True, 60, "miyu-voice attach")
    status = http("GET", "/api/voice/status"); say(f"voice status: {status}")
    results["attached"] = bool(status.get("attached"))
    time.sleep(2)
    # 2/3/4. 唤醒路
    wav_with_keyword = "5.wav"  # 管线 e2e 实测:5.wav 含「周望军」(→「就落实控物价」)
    play(wav_with_keyword)
    ok = wait_for(lambda: any("未有收到" in line for line in notifications()), 30, "「未有收到」通知")
    results["notified_heard"] = ok
    ok = wait_for(lambda: STUB_LOG.exists() and sum(1 for l in STUB_LOG.read_text().splitlines() if '"end"' in l) >= 1, 60, "桩 LLM 完成一次请求")
    results["llm_called"] = ok
    ok = wait_for(lambda: any(line.startswith("-a\tMiyu\t--\t未有\t") for line in notifications()), 30, "完成通知")
    results["notified_done"] = ok
    say("notifications:\n    " + "\n    ".join(notifications()))
    # 语音会话 lane 落库
    marker = HOME / "state" / "voice-session-id"
    results["voice_session_marker"] = marker.exists()
    if marker.exists():
        sid = marker.read_text().strip()
        # 语音会话是 voice kind,不走 WebUI 的会话接口;用 `miyu voice history` 读。
        history = subprocess.run([str(MIYU), "voice", "history", "--limit", "5"], env=env, capture_output=True, text=True, timeout=30)
        count = sum(1 for line in history.stdout.splitlines() if line.rstrip().endswith(" user\x1b[0m"))
        say(f"voice session {sid} turns={count}")
        results["voice_session_turns"] = count
        results["history_has_reply"] = "词元" in history.stdout
    # 语音回合送给模型的用户消息要带 <voice_input> 包裹(语音协议):查隔离库。
    try:
        import sqlite3
        db = sqlite3.connect(str(HOME / "state" / "conversation.db"))
        rows = db.execute("SELECT user_content FROM turns ORDER BY seq DESC LIMIT 3").fetchall(); db.close()
        results["voice_input_wrapped"] = any("<voice_input>" in (row[0] or "") for row in rows)
    except Exception as error:
        say(f"read turns failed: {error}"); results["voice_input_wrapped"] = False
    time.sleep(2)
    # 5. 听写
    sock, frames = ipc({"command": "start_dictation"}, read_events=0, timeout=20)
    say(f"StartDictation reply: {frames}")
    results["dictation_ack"] = bool(frames) and frames[0].get("type") == "ack"
    play("6.wav")
    sock.settimeout(30)
    got = None
    try:
        head = sock.recv(4); size = int.from_bytes(head, "big"); body = b""
        while len(body) < size: body += sock.recv(size - len(body))
        got = json.loads(body)
    except Exception as error:
        say(f"dictation recv failed: {error}")
    say(f"dictation frame: {got}")
    results["dictation_text"] = (got or {}).get("data", {}).get("text")
    sock.close()
    time.sleep(1)
    # 6. 浏览器音频转写(直接把 test wav 转成 16k mono PCM 上传)
    wav16 = WORK / "upload.wav"
    subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-i", str(KWS / "4.wav"), "-ar", "16000", "-ac", "1", "-sample_fmt", "s16", str(wav16)], check=True)
    result = http("POST", "/api/voice/transcribe", wav16.read_bytes(), "audio/wav")
    say(f"transcribe: {result}")
    results["http_transcribe_text"] = result.get("text")
    time.sleep(1)
    # 7. WebSocket 流式听写:按 100ms 一块推 PCM16,尾部补 2s 静音让 VAD 切句
    ws = ws_connect("/api/voice/stream")
    opcode, payload = ws_recv(ws)
    first = json.loads(payload) if opcode == 1 else {}
    say(f"ws first frame: {first}")
    results["ws_ready"] = first.get("type") == "ready"
    pcm = wav16.read_bytes()[44:]
    chunk = 3200  # 100ms @16k PCM16
    for offset in range(0, len(pcm), chunk):
        ws_send(ws, pcm[offset:offset + chunk], 2); time.sleep(0.02)
    for _ in range(20):
        ws_send(ws, bytes(chunk), 2); time.sleep(0.02)
    ws.settimeout(40)
    ws_text = None
    try:
        while True:
            opcode, payload = ws_recv(ws)
            if opcode != 1: continue
            frame = json.loads(payload); say(f"ws frame: {frame}")
            if frame.get("type") == "dictation": ws_text = frame.get("text"); break
            if frame.get("type") in ("ended", "error"): break
    except Exception as error:
        say(f"ws recv failed: {error}")
    results["ws_dictation_text"] = ws_text
    # 继续推静音,daemon 侧 10s 静默应主动回 ended(浏览器不用手动停)
    ws_ended = False
    if ws_text:
        ws.settimeout(0.05); deadline = time.time() + 20
        while time.time() < deadline and not ws_ended:
            ws_send(ws, bytes(chunk), 2)
            try:
                opcode, payload = ws_recv(ws)
                if opcode == 1 and json.loads(payload).get("type") == "ended": ws_ended = True
            except (socket.timeout, TimeoutError):
                pass
            time.sleep(0.05)
        say(f"ws auto-ended after silence: {ws_ended}")
    results["ws_silence_auto_end"] = ws_ended
    try:
        ws_send(ws, b"stop", 1); ws.close()
    except Exception:
        pass
    time.sleep(1)
    ok = wait_for(lambda: http("GET", "/api/voice/status").get("dictating") is False, 10, "听写认领已释放")
    results["ws_released"] = ok
finally:
    say("shutting down")
    try:
        ipc({"command": "shutdown"}, timeout=3)
    except Exception:
        pass
    time.sleep(2)
    if daemon.poll() is None:
        daemon.terminate()
        try: daemon.wait(5)
        except subprocess.TimeoutExpired: daemon.kill()
    stub.terminate()
    leftover = subprocess.run(["pgrep", "-f", f"{VOICE}$"], capture_output=True, text=True).stdout.split()
    results["voice_worker_exited_with_daemon"] = not leftover
    for pid in leftover: subprocess.run(["kill", pid])
    for module in loaded_modules:
        subprocess.run(["pactl", "unload-module", module], capture_output=True)
    print("\n== 结果")
    for key, value in results.items(): print(f"  {key:32} {value}")
    print(f"  日志: {WORK}/daemon.log, {HOME}/state/logs/voice-worker.log")
    if not args.keep and all(results.get(k) for k in ("attached", "notified_heard", "llm_called", "notified_done")):
        pass
