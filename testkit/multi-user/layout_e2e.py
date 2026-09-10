#!/usr/bin/env python3
"""家目录布局(阶段 6)端到端:真二进制 + 隔离 MIYU_HOME。

BIN=<miyu> python3 testkit/multi-user/layout_e2e.py

流程:
  1. 新装起 daemon(带 -p)→ 新布局:home/admin/conversation.db、标记 .home-layout-v1
  2. 建会话、跑一轮(桩模型)、写属主档案;停 daemon
  3. `miyu layout`             → 报「家目录」
  4. `miyu layout --rollback`  → state/conversation.db、data/identities/user-identity.md 回来;标记没了;.home-layout-off 出现
  5. 再起 daemon                → 因为 opt-out 不自动搬;会话与回合仍在(老布局也能跑)
  6. 停;`miyu layout --apply`  → 重新搬成家目录布局;起 daemon → 同一个会话 id、回合还在、档案还在
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
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-multi-user")).expanduser() / "layout"
PORT = int(os.environ.get("PORT", "18493"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18495"))
BASE = f"http://127.0.0.1:{PORT}"
e2e.PORT, e2e.STUB_PORT, e2e.BASE = PORT, STUB_PORT, BASE
HOME = OUT / "home-root"
RUNTIME = OUT / "runtime"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME),
           MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")
e2e.HOME, e2e.RUNTIME, e2e.ENV = HOME, RUNTIME, ENV

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def cli(*args):
    proc = subprocess.run([str(BIN), *args], env=ENV, cwd=str(HOME), capture_output=True, text=True, timeout=60)
    return proc.returncode, proc.stdout + proc.stderr


def start_daemon():
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1"],
                              env=ENV, cwd=str(HOME), stdout=(OUT / "daemon.log").open("a"), stderr=subprocess.STDOUT)
    assert e2e.wait_http(f"{BASE}/api/health"), "daemon not up"
    time.sleep(1)
    return daemon


def stop_daemon(daemon):
    daemon.terminate()
    try:
        daemon.wait(timeout=15)
    except subprocess.TimeoutExpired:
        daemon.kill()
    time.sleep(0.5)


def main():
    import shutil
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    e2e.write_config()
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="plain", STUB_CHUNK_SLEEP="0.01")
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit/webui-fixes/stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        # 1. 新装
        daemon = start_daemon()
        marker = HOME / ".home-layout-v1"
        check("新装即家目录布局", marker.is_file() and marker.read_text().strip() == "admin")
        check("会话库在 home/admin", (HOME / "home/admin/conversation.db").is_file())
        admin = e2e.Client()
        e2e.bootstrap_admin(admin)
        _, created = admin.call("POST", "/api/sessions", {"name": "搬家前的会话"})
        session_id = created["session"]["session_id"]
        view = e2e.run_turn(admin, session_id, "搬家前说一句")
        check("搬家前跑了一轮", len(view.get("turns", [])) == 1)
        status, _ = admin.call("PATCH", "/api/account", {"profile": "我是 shorin"})
        check("写属主档案", status == 200 and (HOME / "home/admin/profile.md").read_text().startswith("我是 shorin"))
        stop_daemon(daemon)
        daemon = None

        # 3. 状态
        code, out = cli("layout")
        check("miyu layout 报家目录布局", code == 0 and "家目录" in out and "home/admin" in out, out.strip()[:120])

        # 4. 回滚
        code, out = cli("layout", "--rollback")
        check("miyu layout --rollback 成功", code == 0, out.strip()[:160])
        check("回滚后会话库回 state", (HOME / "state/conversation.db").is_file() and not (HOME / "home/admin/conversation.db").exists())
        check("回滚后档案回 data/identities/user-identity.md",
              (HOME / "data/identities/user-identity.md").read_text().startswith("我是 shorin") if (HOME / "data/identities/user-identity.md").exists() else False)
        check("回滚后标记没了、opt-out 出现", not marker.exists() and (HOME / ".home-layout-off").is_file())
        code, out = cli("layout")
        check("miyu layout 报老布局+已回滚", code == 0 and "老布局" in out and "*" in out, out.strip()[:160])

        # 5. 回滚后再起 daemon:不自动搬,数据还在
        daemon = start_daemon()
        check("opt-out 生效:没有自动搬回去", not marker.exists() and (HOME / "state/conversation.db").is_file())
        admin = e2e.Client()
        e2e.bootstrap_admin(admin)
        status, view = admin.call("GET", f"/api/sessions/{session_id}/turns")
        check("老布局下会话与回合仍在", status == 200 and len(view.get("turns", [])) == 1, str(status))
        status, me = admin.call("GET", "/api/account")
        check("老布局下档案读的是 user-identity.md", status == 200 and me.get("profile", "").startswith("我是 shorin"), json.dumps(me)[:100])
        stop_daemon(daemon)
        daemon = None

        # 6. 重新搬
        code, out = cli("layout", "--apply")
        check("miyu layout --apply 成功", code == 0 and "home/admin" in out, out.strip()[:160])
        check("apply 后标记回来、opt-out 没了", marker.is_file() and not (HOME / ".home-layout-off").exists())
        daemon = start_daemon()
        admin = e2e.Client()
        e2e.bootstrap_admin(admin)
        status, view = admin.call("GET", f"/api/sessions/{session_id}/turns")
        check("搬家后同一个会话 id、回合还在", status == 200 and len(view.get("turns", [])) == 1, str(status))
        status, me = admin.call("GET", "/api/account")
        check("搬家后档案在 home/admin/profile.md", status == 200 and me.get("profile", "").startswith("我是 shorin")
              and (HOME / "home/admin/profile.md").is_file())
        view2 = e2e.run_turn(admin, session_id, "搬家后再说一句")
        check("搬家后还能跑一轮", len(view2.get("turns", [])) == 2, str(len(view2.get("turns", []))))
    finally:
        if daemon:
            stop_daemon(daemon)
        stub.terminate()
    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
