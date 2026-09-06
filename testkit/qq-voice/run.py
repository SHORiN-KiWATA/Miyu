#!/usr/bin/env python3
"""QQ 语音消息入站 E2E(09-05):假 NapCat 往隔离 daemon 发带 record 段的私聊消息,
daemon 调 get_record 拿 wav(假 NapCat 回 base64)→ miyu-voice 转写 → 正文变成
`[语音] 文本` → 桩 LLM 回复。语音关着时应静默退化成 `[语音消息]` 占位。

隔离:MIYU_HOME + XDG_RUNTIME_DIR;桩 LLM;miyu-voice 的麦克风被 PIPEWIRE_NODE
引到 null sink(不碰真实麦克风)。

用法: testkit/qq-voice/run.py [--bin-dir target/release] [--no-voice] [--wav 文件]
判据(脚本末尾打印):
  api 序列含 get_record;桩 LLM 收到请求;conversation.db 最新 user_content 含
  「[语音] …」(--no-voice 时含「[语音消息]」且 api 序列不含 get_record)。
"""
import argparse, base64, json, os, re, shutil, socket, struct, subprocess, sys, threading, time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "testkit" / "persona-ab"))
from run import strip_jsonc  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--bin-dir", default=str(REPO / "target" / "release"))
ap.add_argument("--no-voice", action="store_true")
ap.add_argument("--wav", default=str(REPO / "testkit/voice/samples/miyou.wav"))
ap.add_argument("--keep", action="store_true")
args = ap.parse_args()
BIN_DIR = Path(args.bin_dir).resolve()
MIYU, VOICE = BIN_DIR / "miyu", BIN_DIR / "miyu-voice"
assert MIYU.is_file(), f"缺二进制: {MIYU}"

WORK = Path("/tmp/miyu-qq-voice")
HOME, RUN_DIR = WORK / "home", WORK / "run"
STUB_LOG = WORK / "stub.jsonl"
STUB_PORT, WEB_PORT, WS_PORT = 18497, 18496, 18498
SINK = "miyu_qqvoice_sink"
MODELS = Path(os.environ.get("MIYU_VOICE_MODELS_DIR", Path.home() / ".miyu/state/models"))
REAL_CONFIG = Path.home() / ".miyu/config/config.jsonc"
SELF_ID, SENDER = 900000001, 800000043
WAV = Path(args.wav)
t0 = time.time()
def say(msg): print(f"{time.time()-t0:7.2f}s {msg}", flush=True)

if WORK.exists(): shutil.rmtree(WORK)
for d in (HOME / "config", HOME / "state", RUN_DIR): d.mkdir(parents=True)
(HOME / "state" / "models").symlink_to(MODELS)
real = json.loads(strip_jsonc(REAL_CONFIG.read_text()), strict=False)
cfg = dict(real)
for key in ("web", "alarm", "mcp"): cfg.pop(key, None)
cfg["providers"] = [{"enabled": True, "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
                     "protocol": "openai-chat", "api_key": "stub-key", "models": ["stub-model"]}]
cfg["active_provider"] = "stub"
cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-model"}]
cfg.pop("active_multimodal_provider_models", None)
cfg.setdefault("memory", {})["enabled"] = False
cfg.setdefault("cache", {})["request_log"] = False
cfg.setdefault("notifications", {})["enabled"] = False
qq = cfg.setdefault("platforms", {}).setdefault("qq", {})
qq.update({"enabled": True, "reverse_ws_port": WS_PORT, "access_token": "", "text_models": None,
           "text_models_inheritance": "global"})
qq.pop("multimodal_models", None)
qq.setdefault("private_chats", {})["allow_non_whitelist"] = True
cfg["voice"] = {"enabled": not args.no_voice, "wake_keywords": ["未有未有"], "follow_up_seconds": 0,
                "stt_unload_seconds": 300, "stt_language": "zh", "sounds": False,
                "tts": {"enabled": False}}
(HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2))

mods = subprocess.run(["pactl", "list", "short", "modules"], capture_output=True, text=True).stdout
loaded_modules = []  # 本脚本加载的 null sink,结束时卸掉(留着会污染用户的 PipeWire 图)
if SINK not in mods:
    r = subprocess.run(["pactl", "load-module", "module-null-sink", f"sink_name={SINK}", "channel_map=front-left,front-right"], check=True, capture_output=True, text=True)
    loaded_modules.append(r.stdout.strip())
env = dict(os.environ)
for k in ("XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "MIYU_DIRECT"): env.pop(k, None)
env.update(MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUN_DIR), LANG="zh_CN.UTF-8", MIYU_LOG="info",
           PIPEWIRE_RUNTIME_DIR=os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"),
           PIPEWIRE_NODE=SINK, PIPEWIRE_PROPS="{ stream.capture.sink = true }")
stub = subprocess.Popen([sys.executable, str(REPO / "testkit/low-footprint/stub_llm.py")],
                        env={**os.environ, "STUB_PORT": str(STUB_PORT), "STUB_LOG": str(STUB_LOG), "STUB_CHUNKS": "4", "STUB_DELAY_MS": "20"},
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
daemon_log = open(WORK / "daemon.log", "w")
daemon = subprocess.Popen([str(MIYU), "__daemon", "--port", str(WEB_PORT), "--bind", "127.0.0.1"], env=env,
                          stdout=daemon_log, stderr=subprocess.STDOUT, preexec_fn=os.setsid)

def http(path):
    import urllib.request
    with urllib.request.urlopen(f"http://127.0.0.1:{WEB_PORT}{path}", timeout=30) as resp:
        return json.loads(resp.read())

def wait_for(pred, timeout, what):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            if pred(): return True
        except Exception:
            pass
        time.sleep(0.3)
    say(f"!! 超时: {what}"); return False

def socket_path():
    found = sorted(RUN_DIR.glob("miyu*/core.sock"))
    return found[0] if found else None

# ---------- 假 NapCat(反向 ws 客户端) ----------
class WS:
    def __init__(self, sock): self.sock, self.buf = sock, b""
    @classmethod
    def connect(cls):
        sock = socket.create_connection(("127.0.0.1", WS_PORT), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall((f"GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{WS_PORT}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
                      f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nX-Self-ID: {SELF_ID}\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            ch = sock.recv(4096)
            if not ch: raise RuntimeError("握手时连接被关闭")
            head += ch
        assert b" 101 " in head.split(b"\r\n", 1)[0], head[:80]
        sock.settimeout(None); return cls(sock)
    def send(self, obj):
        p = json.dumps(obj, ensure_ascii=False).encode(); mask = os.urandom(4)
        h = bytearray([0x81])
        if len(p) < 126: h.append(0x80 | len(p))
        elif len(p) < 65536: h.append(0x80 | 126); h += struct.pack("!H", len(p))
        else: h.append(0x80 | 127); h += struct.pack("!Q", len(p))
        self.sock.sendall(bytes(h) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(p)))
    def _fill(self, n):
        while len(self.buf) < n:
            ch = self.sock.recv(65536)
            if not ch: raise ConnectionError("连接已关闭")
            self.buf += ch
    def recv(self):
        self._fill(2); b1, b2 = self.buf[0], self.buf[1]; ln, off = b2 & 0x7F, 2
        if ln == 126: self._fill(4); ln = struct.unpack("!H", self.buf[2:4])[0]; off = 4
        elif ln == 127: self._fill(10); ln = struct.unpack("!Q", self.buf[2:10])[0]; off = 10
        self._fill(off + ln); payload, self.buf = self.buf[off:off + ln], self.buf[off + ln:]
        op = b1 & 0x0F
        if op == 8: raise ConnectionError("对端关闭")
        if op == 9: return None
        return json.loads(payload.decode(errors="replace")) if payload else None

REPLIES, API_LOG = [], []
def api_data(action, params):
    if action in ("send_group_msg", "send_msg", "send_private_msg"): return {"message_id": int(time.time() * 1000) % 2**31}
    if action == "get_login_info": return {"user_id": SELF_ID, "nickname": "Miyu"}
    if action == "get_record":
        assert params.get("out_format") == "wav", params
        return {"file": "/nonexistent/on/bridge/host.wav", "base64": base64.b64encode(WAV.read_bytes()).decode()}
    if action == "get_msg": return {"message_id": params.get("message_id"), "message": [], "sender": {"user_id": SENDER, "nickname": "测试私聊"}}
    return {}

def pump(ws):
    while True:
        try: fr = ws.recv()
        except (ConnectionError, OSError) as e: say(f"[pump] 连接结束: {e}"); return
        if not isinstance(fr, dict) or "action" not in fr: continue
        action, params = fr["action"], fr.get("params", {})
        API_LOG.append(action)
        if action in ("send_group_msg", "send_msg", "send_private_msg"):
            REPLIES.append(params.get("message")); say(f"← Miyu 回复: {render(params.get('message'))}")
        else:
            say(f"[api] {action} {json.dumps({k: (v[:40] + '…' if isinstance(v, str) and len(v) > 40 else v) for k, v in params.items()}, ensure_ascii=False)}")
        ws.send({"status": "ok", "retcode": 0, "data": api_data(action, params), "echo": fr.get("echo")})

def render(m):
    if isinstance(m, str): return m
    return "".join(seg["data"].get("text", "") if seg.get("type") == "text" else f"[{seg.get('type')}]" for seg in m or []).strip()

results = {}
try:
    assert wait_for(lambda: socket_path() is not None, 20, "daemon socket") and daemon.poll() is None
    if not args.no_voice:
        assert wait_for(lambda: http("/api/voice/status").get("attached") is True, 60, "miyu-voice attach")
        time.sleep(1)
    ws = WS.connect(); threading.Thread(target=pump, args=(ws,), daemon=True).start()
    time.sleep(1.5)
    mid = int(time.time() * 1000) % 2**31
    ws.send({"post_type": "message", "message_type": "private", "sub_type": "friend", "self_id": SELF_ID, "user_id": SENDER,
             "message_id": mid, "raw_message": "[CQ:record,file=abc123.amr]", "font": 0, "time": int(time.time()),
             "message": [{"type": "record", "data": {"file": "abc123.amr", "file_size": "4096"}}],
             "sender": {"user_id": SENDER, "nickname": "测试私聊"}})
    say("→ 发了一条纯语音私聊消息")
    got = wait_for(lambda: len(REPLIES) > 0, 90, "Miyu 回复")
    results["replied"] = got
    results["api_has_get_record"] = "get_record" in API_LOG
    results["llm_called"] = STUB_LOG.exists() and '"end"' in STUB_LOG.read_text()
    time.sleep(1)
    import sqlite3
    db = sqlite3.connect(str(HOME / "state" / "conversation.db"))
    rows = db.execute("SELECT user_content FROM turns ORDER BY seq DESC LIMIT 1").fetchall(); db.close()
    content = rows[0][0] if rows else ""
    results["user_content"] = content.strip()[:200]
    results["has_transcript"] = "[语音] " in content
    results["has_placeholder"] = "[语音消息]" in content
finally:
    try:
        sock = socket.socket(socket.AF_UNIX); sock.settimeout(3); sock.connect(str(socket_path()))
        payload = json.dumps({"version": 3, "command": "shutdown"}).encode()
        sock.sendall(len(payload).to_bytes(4, "big") + payload); sock.close()
    except Exception:
        pass
    time.sleep(2)
    if daemon.poll() is None:
        daemon.terminate()
        try: daemon.wait(5)
        except subprocess.TimeoutExpired: daemon.kill()
    stub.terminate()
    for pid in subprocess.run(["pgrep", "-f", f"{VOICE}$"], capture_output=True, text=True).stdout.split():
        subprocess.run(["kill", pid])
    for module in loaded_modules:
        subprocess.run(["pactl", "unload-module", module], capture_output=True)
    print(f"\n== 结果 ({'语音关' if args.no_voice else '语音开'})")
    for key, value in results.items(): print(f"  {key:22} {value}")
    print(f"  API 序列: {API_LOG}")
    print(f"  日志: {WORK}/daemon.log, {HOME}/cache/logs/")
