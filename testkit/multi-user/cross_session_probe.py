#!/usr/bin/env python3
"""会话 A 正在跑一轮时,对会话 B 的各种命令该照常工作(09-10 用户反馈:
「有会话在运行的时候我在另一个会话里无法 reset」)。隔离 daemon + 桩模型,接口层。

BIN=<miyu> python3 testkit/multi-user/cross_session_probe.py
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
import e2e  # noqa: E402

BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-multi-user")).expanduser() / "cross"
PORT = int(os.environ.get("PORT", "18494"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18490"))
BASE = f"http://127.0.0.1:{PORT}"
e2e.PORT, e2e.STUB_PORT, e2e.BASE = PORT, STUB_PORT, BASE
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME),
           MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")
e2e.HOME, e2e.RUNTIME, e2e.ENV = HOME, RUNTIME, ENV

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def main():
    import shutil
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    e2e.write_config()
    password_file = OUT / "web-password"
    password_file.write_text(e2e.ADMIN_PASSWORD + "\n")
    # 长回复:70 行 × 0.15s ≈ 10s,足够在中间对别的会话动手
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="long", STUB_CHUNK_SLEEP="0.12", STUB_LINES="400")
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit/webui-fixes/stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1",
                                   "--password-file", str(password_file)],
                                  env=ENV, cwd=str(HOME), stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert e2e.wait_http(f"{BASE}/api/health"), "daemon not up"
        time.sleep(1)
        admin = e2e.Client()
        assert admin.login(e2e.ADMIN_PASSWORD)[0] == 204
        _, a = admin.call("POST", "/api/sessions", {"name": "A 在跑"})
        _, b = admin.call("POST", "/api/sessions", {"name": "B 被操作"})
        sid_a, sid_b = a["session"]["session_id"], b["session"]["session_id"]
        # A 开跑
        status, data = admin.call("POST", "/api/turns", {"content": "A 开始长回复", "session_id": sid_a})
        check("A 起回合", status in (200, 201, 202), f"{status} {data}")
        time.sleep(1.5)
        status, view = admin.call("GET", f"/api/sessions/{sid_a}/turns")
        check("A 确实在跑", status == 200 and bool(view.get("runs")), json.dumps(view.get("runs"))[:100])

        # B 上的操作
        status, data = admin.call("POST", "/api/conversation/reset", {"session_id": sid_b})
        check("A 跑着时 reset B", status in (200, 204), f"{status} {data}")
        status, data = admin.call("PATCH", f"/api/sessions/{sid_b}", {"name": "B 改名"})
        check("A 跑着时改名 B", status == 200, f"{status} {data}")
        status, data = admin.call("POST", "/api/turns", {"content": "B 也来一轮 short", "session_id": sid_b})
        check("A 跑着时 B 起回合", status in (200, 201, 202), f"{status} {data}")
        status, data = admin.call("POST", "/api/conversation/compact", {"session_id": sid_b})
        # B 那轮很短,可能已经跑完:跑着就该 409,跑完就该 200——两者都是对的
        check("B 自己在跑时 compact B 被拒(409)/跑完则 200", status in (200, 409), f"{status} {data}")
        # 等 B 结束(桩 400 行 × 0.12s ≈ 48s)
        deadline = time.time() + 120
        while time.time() < deadline:
            status, view = admin.call("GET", f"/api/sessions/{sid_b}/turns")
            if status == 200 and not view.get("runs"):
                break
            time.sleep(0.5)
        status, view_a = admin.call("GET", f"/api/sessions/{sid_a}/turns")
        a_running = bool(view_a.get("runs"))
        check("此时 A 仍在跑(前提)", a_running, json.dumps(view_a.get("runs"))[:80])
        status, data = admin.call("POST", "/api/conversation/compact", {"session_id": sid_b})
        check("A 跑着时 compact B(空闲)", status == 200, f"{status} {data}")
        if status != 200:
            _, boot = admin.call("GET", "/api/bootstrap")
            print("DEBUG runs:", json.dumps(boot.get("runs")), "active:", boot.get("active_run_id"))
            _, vb = admin.call("GET", f"/api/sessions/{sid_b}/turns")
            print("DEBUG B runs:", json.dumps(vb.get("runs")), "running_turn:", vb.get("running_turn_id"))
        status, data = admin.call("GET", f"/api/sessions/{sid_b}/pop/candidates")
        status2, data2 = admin.call("POST", "/api/conversation/pop", {"session_id": sid_b, "count": 1})
        check("A 跑着时 pop B(不被 busy 挡)", status2 in (200, 204) or "evictable" in json.dumps(data2), f"candidates={status} pop={status2} {data2}")
        status, data = admin.call("PUT", f"/api/sessions/{sid_b}/models", {"models": [{"provider_id": "stub", "model": "stub-a"}]})
        check("A 跑着时改 B 的会话模型", status == 200, f"{status} {data}")
        status, data = admin.call("PUT", "/api/models/active", {"models": [{"provider_id": "stub", "model": "stub-a"}]})
        check("A 跑着时改全局模型", status in (200, 204), f"{status} {data}")
        status, data = admin.call("POST", "/api/sessions", {"name": "C 新建"})
        check("A 跑着时新建会话", status == 201, f"{status}")
        sid_c = data["session"]["session_id"]
        status, data = admin.call("DELETE", f"/api/sessions/{sid_c}")
        check("A 跑着时删会话 C", status == 200, f"{status} {data}")
        status, data = admin.call("DELETE", f"/api/sessions/{sid_b}")
        check("A 跑着时删会话 B", status == 200, f"{status} {data}")
        status, data = admin.call("POST", "/api/conversation/reset", {"session_id": sid_a})
        check("A 自己在跑时 reset A 被拒(409)", status == 409, f"{status} {data}")
        # 等 A 结束
        deadline = time.time() + 60
        while time.time() < deadline:
            status, view = admin.call("GET", f"/api/sessions/{sid_a}/turns")
            if status == 200 and not view.get("runs"):
                break
            time.sleep(0.5)
        check("A 跑完", not view.get("runs") and view.get("turns"), str(len(view.get("turns", []))))
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        stub.terminate()
    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
