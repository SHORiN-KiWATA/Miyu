#!/usr/bin/env python3
"""成员工具走查(09-11):隔离 daemon + 会叫工具的桩模型,成员(私有人格,勾了脚本
e2e_hello)跑一轮,把脚本工具、read、glob、edit、run_command、print_image 全叫一遍,看:
- 时间线里每个工具的 display_name(用户反馈「显示名没生效」)
- 沙盒:读/写工作区之外(~/.miyu/config、/etc)必须被拒,工作区内正常
- print_image 的图片资源成员自己能取到(用户反馈「图片加载失败」)
管理员同一套再跑一遍作对照(不套沙盒)。

    BIN=<miyu> python3 testkit/multi-user/member_tools_probe.py
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

PORT = int(os.environ.get("PORT", "18552"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18556"))
OUT = Path("~/.cache/miyu-member-tools").expanduser()
PNG = bytes.fromhex("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da63f8cfc000000301010018dd8db00000000049454e44ae426082")
results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def calls_for(home, workspace):
    secret = f"{home}/config/secret.txt"
    return [
        {"name": "e2e_hello", "args": {}},
        {"name": "read", "args": {"path": f"{workspace}/note.txt"}},
        {"name": "read", "args": {"path": secret}},
        {"name": "read", "args": {"path": "/etc/hostname"}},
        {"name": "glob", "args": {"pattern": "*.txt", "path": f"{home}/config"}},
        {"name": "edit", "args": {"patchText": f"*** Begin Patch\n*** Add File: {secret}.new\n+pwned\n*** End Patch\n"}},
        {"name": "edit", "args": {"patchText": f"*** Begin Patch\n*** Add File: {workspace}/made.txt\n+ok\n*** End Patch\n"}},
        {"name": "run_command", "args": {"command": f"cat {secret} 2>&1 | head -1; echo sandboxed > {workspace}/cmd.txt && echo WROTE"}},
        {"name": "print_image", "args": {"image": f"{workspace}/pic.png"}},
    ]


def raw_get(client, path):
    """二进制资源:只要状态码,不解析 JSON。"""
    import http.client
    conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
    conn.request("GET", path, headers={"cookie": client.cookie} if client.cookie else {})
    resp = conn.getresponse()
    resp.read()
    return resp.status


def run_actor(client, sid, label, home, workspace, sandboxed):
    view = e2e.run_turn(client, sid, "把工具都试一遍")
    turn = view["turns"][-1]
    calls = [c for r in turn.get("tool_flow", []) for c in r.get("calls", [])]
    names = {}
    for c in calls:
        names.setdefault(c["name"], []).append(c)
    print(f"== {label}: {len(calls)} calls")
    for c in calls:
        print(f"   {c['name']:14s} display={c['display_name']!r:16s} ok={c['ok']} out={c['output'][:70].replace(chr(10), ' ')!r}")
    check(f"{label}: 脚本工具显示名", any(c["display_name"] == "打招呼" for c in names.get("e2e_hello", [])),
          json.dumps([c["display_name"] for c in names.get("e2e_hello", [])], ensure_ascii=False))
    check(f"{label}: read 显示名不是裸 id", all(c["display_name"] != "read" for c in names.get("read", [])),
          json.dumps([c["display_name"] for c in names.get("read", [])], ensure_ascii=False))
    reads = names.get("read", [])
    check(f"{label}: 读工作区文件成功", reads and reads[0]["ok"], reads[0]["output"][:60] if reads else "no call")
    if sandboxed:
        check(f"{label}: 读 config/secret 被拒", len(reads) > 1 and not reads[1]["ok"] and "sandbox" in reads[1]["output"], reads[1]["output"][:80] if len(reads) > 1 else "")
        # 系统目录(/etc /usr …)只读放行:跑程序离不开;私人的家与 ~/.miyu 才是要挡的
        check(f"{label}: 读 /etc/hostname 放行(系统目录只读)", len(reads) > 2 and reads[2]["ok"], reads[2]["output"][:80] if len(reads) > 2 else "")
        globs = names.get("glob", [])
        check(f"{label}: glob config 目录被拒", globs and not globs[0]["ok"], globs[0]["output"][:80] if globs else "")
        edits = names.get("edit", [])
        check(f"{label}: edit 写 config 之外被拒", edits and not edits[0]["ok"] and "sandbox" in edits[0]["output"], edits[0]["output"][:80] if edits else "")
        check(f"{label}: edit 写工作区成功", len(edits) > 1 and edits[1]["ok"] and Path(f"{workspace}/made.txt").is_file(), edits[1]["output"][:80] if len(edits) > 1 else "")
        cmds = names.get("run_command", [])
        out = cmds[0]["output"] if cmds else ""
        denied = "Permission denied" in out or "权限不够" in out
        check(f"{label}: run_command 读 secret 被拒、写工作区成功", denied and "TOP-SECRET" not in out and "WROTE" in out and Path(f"{workspace}/cmd.txt").is_file(), out[:120].replace("\n", " "))
        check(f"{label}: 沙盒外没被写", not Path(f"{home}/config/secret.txt.new").exists())
    else:
        check(f"{label}: 管理员读 config/secret 不受限", len(reads) > 1 and reads[1]["ok"], reads[1]["output"][:60] if len(reads) > 1 else "")
        Path(f"{home}/config/secret.txt.new").unlink(missing_ok=True)
    prints = names.get("print_image", [])
    check(f"{label}: print_image 成功", prints and prints[0]["ok"], prints[0]["output"][:80] if prints else "")
    assets = turn.get("assets", [])
    check(f"{label}: 回合带图片资源", bool(assets), json.dumps(assets)[:100])
    if assets:
        url = assets[0]["url"]
        status = raw_get(client, url)
        check(f"{label}: 图片资源 GET {url[:30]}… 200", status == 200, str(status))
    return calls


def ui_phase_badge(home, sid, member, workspace):
    """页面开着的时候跑一轮:失败的 edit 卡片上不该留着「准备修改」阶段签(09-11 手机端实测)。"""
    from playwright.sync_api import sync_playwright
    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        page = browser.new_page(viewport={"width": 390, "height": 844})
        page.goto(e2e.BASE)
        page.wait_for_selector("#loginForm:not([hidden])", timeout=15000)
        page.fill("#loginUsername", "alice")
        page.fill("#loginPassword", "alice-pass")
        page.click("#loginSubmit")
        page.wait_for_function("() => !document.body.classList.contains('is-blocked')", timeout=20000)
        if page.evaluate("() => { const o = document.getElementById('oobe'); return Boolean(o && !o.hidden); }"):
            page.click("#oobeSkip")
        page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
        view = e2e.run_turn(member, sid, "把工具都试一遍")
        page.wait_for_timeout(1500)
        cards = page.evaluate("""() => [...document.querySelectorAll('.tool-card')].map((card) => ({
            title: (card.querySelector('.tool-display-name, .tool-name, .tool-title') || card).textContent.trim().slice(0, 40),
            failed: card.className.includes('failure') || Boolean(card.querySelector('.is-failure')),
            live: [...card.querySelectorAll('.tool-live-progress')].map((el) => ({text: el.textContent.trim(), hidden: el.hidden})),
        }))""")
        edits = [c for c in cards if "编辑" in c["title"]]
        print("edit cards:", json.dumps(edits, ensure_ascii=False)[:400])
        failed = [c for c in edits if c["failed"]]
        check("失败的 edit 卡片上没有挂着「准备修改」", failed and all(not (l["text"] == "准备修改" and not l["hidden"]) for c in failed for l in c["live"]),
              json.dumps(failed, ensure_ascii=False)[:200])
        page.screenshot(path=str(OUT / "phase-badge.png"))
        browser.close()


def main():
    import shutil
    if OUT.exists():
        shutil.rmtree(OUT)
    e2e.PORT, e2e.STUB_PORT, e2e.BASE = PORT, STUB_PORT, f"http://127.0.0.1:{PORT}"
    e2e.OUT = OUT
    e2e.HOME = OUT / "home"
    e2e.RUNTIME = OUT / "runtime"
    e2e.ENV = dict(os.environ, MIYU_HOME=str(e2e.HOME), XDG_RUNTIME_DIR=str(e2e.RUNTIME),
                   MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")
    HOME = e2e.HOME
    HOME.mkdir(parents=True)
    e2e.RUNTIME.mkdir(parents=True)
    e2e.write_config()
    (HOME / "config/secret.txt").write_text("TOP-SECRET\n")
    script_dir = HOME / "extensions/scripts"
    script_dir.mkdir(parents=True)
    (script_dir / "e2e_hello.py").write_text(
        "#!/usr/bin/env python3\n# Display name: 打招呼\n# Description: e2e sample script\n# Timeout: 5\n# Permission: read-only\n"
        "# Parameters:\n# {\"type\": \"object\", \"properties\": {}}\nprint('hi')\n", "utf-8")
    (script_dir / "e2e_hello.py").chmod(0o755)
    member_ws = HOME / "home/alice/workspace"
    admin_ws = HOME / "home/admin/workspace"
    for ws in (member_ws, admin_ws):
        ws.mkdir(parents=True)
        (ws / "note.txt").write_text("hello from workspace\n")
        (ws / "pic.png").write_bytes(PNG)
    member_calls = calls_for(HOME, member_ws)
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), STUB_CALLS=json.dumps(member_calls))
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_member_tools.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(e2e.BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1"],
                                  env=e2e.ENV, cwd=str(HOME), stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert e2e.wait_http(f"{e2e.BASE}/api/health"), "daemon not up"
        time.sleep(1)
        admin = e2e.bootstrap_admin(e2e.Client())
        status, invite = admin.call("POST", "/api/admin/invites", {})
        member = e2e.Client()
        status, _ = member.call("POST", "/api/auth/register", {"invite": invite["code"], "username": "alice", "display_name": "", "password": "alice-pass"})
        assert status == 204, status
        status, data = member.call("POST", "/api/account/personas",
                                   {"name": "小满", "description": "", "prompt": "你是小满。", "memory": False,
                                    "plugins": ["knowledge_base"], "scripts": ["e2e_hello"], "activate": True})
        assert status == 201, (status, data)
        status, boot = member.call("GET", "/api/bootstrap")
        sid = boot["current_session_id"]
        if os.environ.get("UI") == "1":
            ui_phase_badge(HOME, sid, member, str(member_ws))
        else:
            run_actor(member, sid, "member", str(HOME), str(member_ws), sandboxed=True)

        # 管理员对照:同样的调用,但路径指向管理员工作区(工作区=会话 workspace,这里直接用 HOME)
        status, created = admin.call("POST", "/api/sessions", {"name": "管理员走查", "workspace": str(admin_ws)})
        asid = created["session"]["session_id"]
        stub.terminate()
        stub.wait(timeout=5)
        stub_env["STUB_CALLS"] = json.dumps(calls_for(HOME, admin_ws))
        stub2 = subprocess.Popen([sys.executable, str(HERE / "stub_member_tools.py")], env=stub_env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub2 not up"
        try:
            run_actor(admin, asid, "admin", str(HOME), str(admin_ws), sandboxed=False)
        finally:
            stub2.terminate()
    finally:
        for proc in (daemon, stub):
            if proc:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except Exception:
                    proc.kill()
    failed = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
