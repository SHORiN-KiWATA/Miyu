#!/usr/bin/env python3
"""claude-code 中转线「准备xx」WebUI 侧真机验证(花真实订阅额度,不进 CI)。

隔离 home 起 daemon(默认 8400)→ 订阅 /api/events SSE → 建会话 → 发一轮让
claude 用原生 Bash/Write → 断言收到带 phase 的 tool.preparing,且早于对应的
tool.started。未配 WebUI 密码时 loopback 免登录。

    MIYU_HOME=/tmp/miyu-ccprep/home python3 testkit/claude-code/preparing_web.py

09-06 实测(haiku):Bash 准备执行→started 0.3s;Write 准备编辑→started 8.7s。
"""

import json
import os
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = os.environ.get("MIYU_HOME", "/tmp/miyu-ccprep/home")
PORT = int(os.environ.get("MIYU_PROBE_PORT", "8400"))
BASE = f"http://127.0.0.1:{PORT}"
RUNTIME = os.environ.get("XDG_RUNTIME_DIR_PROBE", "/tmp/mx-ccprep")
ENV = dict(os.environ, MIYU_HOME=HOME, XDG_RUNTIME_DIR=RUNTIME)
Path(RUNTIME).mkdir(exist_ok=True)
WORK = Path(os.environ.get("MIYU_PROBE_WORK", "/tmp/miyu-ccprep/work"))
WORK.mkdir(parents=True, exist_ok=True)

PROMPT = (
    "测试任务,不要提问直接做:先用 Bash 运行 `ls -la`;"
    "然后用 Write 工具把一个约 60 行、带详细中文注释的 Rust 快速排序程序写到 qs.rs;"
    "最后用 Bash 运行 `wc -l qs.rs`。做完只回一个字:好。"
)


def daemon(cmd, *extra):
    return subprocess.run(
        [str(BIN), "daemon", cmd, *extra], env=ENV, cwd=WORK, capture_output=True, text=True
    )


def api(path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE + path,
        data=data,
        method="POST" if data else "GET",
        headers={"Content-Type": "application/json", "Origin": BASE},
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read() or b"null")


def main():
    started = daemon("start", "--port", str(PORT))
    print("daemon start:", (started.stdout + started.stderr).strip().splitlines()[:1])
    for _ in range(30):
        try:
            api("/api/bootstrap")
            break
        except Exception:
            time.sleep(1)

    events = []
    stop = threading.Event()

    def sse():
        req = urllib.request.Request(BASE + "/api/events", headers={"Origin": BASE})
        with urllib.request.urlopen(req, timeout=600) as resp:
            kind = None
            for raw in resp:
                line = raw.decode("utf-8", "replace").rstrip("\n")
                if line.startswith("event:"):
                    kind = line[6:].strip()
                elif line.startswith("data:"):
                    events.append((time.time(), kind, line[5:].strip()))
                if stop.is_set():
                    break

    threading.Thread(target=sse, daemon=True).start()
    time.sleep(1)
    session = api("/api/sessions", {"name": "cc-preparing", "switch": True, "mode": "normal"})
    sid = session["session"]["session_id"]
    print("turn:", api("/api/turns", {"content": PROMPT, "session_id": sid}))

    deadline = time.time() + 240
    while time.time() < deadline:
        if any(k == "run.completed" for _, k, _ in events):
            break
        time.sleep(1)
    time.sleep(1)
    stop.set()

    t0 = events[0][0] if events else time.time()
    ok = False
    started_names = []
    for t, k, d in events:
        if k in ("tool.preparing", "tool.started", "tool.finished"):
            v = json.loads(d)
            print(f"{t - t0:6.2f}s {k:14} name={v.get('name')} phase={v.get('phase')} batch={v.get('batch')}")
            if k == "tool.started":
                started_names.append(v.get("name"))
            if k == "tool.preparing" and v.get("phase") and v.get("name") not in started_names:
                ok = True
    print("PASS" if ok else "FAIL")
    print("daemon stop:", daemon("stop").stdout.strip()[-80:])
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
