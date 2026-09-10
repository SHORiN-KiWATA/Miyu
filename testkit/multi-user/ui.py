#!/usr/bin/env python3
"""多用户浏览器走查:登录页(用户名+密码/注册表单)、成员看不到管理面板、账号页。

BIN=<miyu> python3 testkit/multi-user/ui.py     # 截图落 ~/.cache/miyu-multi-user/ui-*.png
"""
import json
import os
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
import e2e  # noqa: E402  复用隔离 daemon 的准备

BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-multi-user")).expanduser()
PORT = int(os.environ.get("PORT", "18492"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18496"))
BASE = f"http://127.0.0.1:{PORT}"
e2e.PORT, e2e.STUB_PORT, e2e.BASE = PORT, STUB_PORT, BASE
e2e.OUT = OUT / "ui"
e2e.HOME = e2e.OUT / "home"
e2e.RUNTIME = e2e.OUT / "runtime"
e2e.ENV = dict(os.environ, MIYU_HOME=str(e2e.HOME), XDG_RUNTIME_DIR=str(e2e.RUNTIME),
               MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def main():
    import shutil
    if e2e.OUT.exists():
        shutil.rmtree(e2e.OUT)
    e2e.HOME.mkdir(parents=True)
    e2e.RUNTIME.mkdir(parents=True)
    e2e.write_config()
    password_file = e2e.OUT / "web-password"
    password_file.write_text(e2e.ADMIN_PASSWORD + "\n")
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="plain", STUB_CHUNK_SLEEP="0.01")
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit/webui-fixes/stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1",
                                   "--password-file", str(password_file)],
                                  env=e2e.ENV, cwd=str(e2e.HOME), stdout=(e2e.OUT / "daemon.log").open("w"),
                                  stderr=subprocess.STDOUT)
        assert e2e.wait_http(f"{BASE}/api/health"), "daemon not up"
        time.sleep(1)
        admin = e2e.Client()
        assert admin.login(e2e.ADMIN_PASSWORD)[0] == 204
        admin.call("POST", "/api/sessions", {"name": "管理员的会话"})
        _, invite = admin.call("POST", "/api/admin/invites", {})
        code = invite["code"]

        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 860})
            page.goto(BASE)
            page.wait_for_selector("#loginForm:not([hidden])", timeout=15000)
            check("登录页有用户名框", page.is_visible("#loginUsername"))
            page.screenshot(path=str(OUT / "ui-login.png"))
            page.click("#showRegisterButton")
            page.wait_for_selector("#registerForm:not([hidden])")
            check("注册表单出现", page.is_visible("#registerInvite"))
            page.fill("#registerInvite", code)
            page.fill("#registerUsername", "alice")
            page.fill("#registerDisplayName", "爱丽丝")
            page.fill("#registerPassword", "alice-pass")
            page.screenshot(path=str(OUT / "ui-register.png"))
            page.click("#registerSubmit")
            # 注册成功即打开账号页(OOBE 第二步:希望 AI 如何认知你)
            page.wait_for_selector("#consoleView:not([hidden])", timeout=20000)
            page.wait_for_timeout(1200)
            account_open = page.evaluate("() => document.querySelector('.con-panel[data-console-panel=\"account\"]').hidden === false")
            check("注册成功自动打开账号页", account_open is True, str(account_open))
            settings_hidden = page.evaluate("() => document.getElementById('sidebarSettingsButton').hidden")
            check("成员侧栏没有设置按钮", settings_hidden is True, str(settings_hidden))
            sessions = page.evaluate("() => [...document.querySelectorAll('#sessionItems [data-session-id]')].map(e => e.textContent)")
            check("成员侧栏看不到管理员的会话", not any("管理员的会话" in text for text in sessions), json.dumps(sessions, ensure_ascii=False))
            visible_rail = page.evaluate("() => [...document.querySelectorAll('.con-rail-item[data-console-panel]')].filter(e => !e.hidden).map(e => e.dataset.consolePanel)")
            check("成员控制台只剩数据统计与账号", sorted(visible_rail) == ["account", "usage"], json.dumps(visible_rail))
            page.screenshot(path=str(OUT / "ui-member-console.png"))
            page.fill("#accountProfile", "请叫我爱丽丝")
            page.click("#accountSave")
            page.wait_for_timeout(800)
            profile_saved = (e2e.HOME / "home/alice/profile.md").read_text().strip() if (e2e.HOME / "home/alice/profile.md").exists() else ""
            check("账号页写档案落到 home/alice/profile.md", profile_saved == "请叫我爱丽丝", profile_saved)
            username = page.input_value("#accountUsername")
            check("账号页显示用户名", username == "alice", username)
            invite_card_hidden = page.evaluate("() => [...document.querySelectorAll('[data-console-panel=\"account\"] [data-admin-only]')].every(e => e.hidden)")
            check("成员账号页没有邀请码/成员卡", invite_card_hidden is True)
            page.screenshot(path=str(OUT / "ui-member-account.png"))
            page.click("#accountLogout")
            page.wait_for_selector("#loginForm:not([hidden])", timeout=10000)
            check("退出登录回到登录页", True)

            # 管理员:账号页有邀请码与成员表
            page.fill("#loginUsername", "admin")
            page.fill("#loginPassword", e2e.ADMIN_PASSWORD)
            page.click("#loginSubmit")
            page.wait_for_selector("#consoleButton", timeout=20000)
            page.wait_for_timeout(1000)
            page.click("#consoleButton")
            page.wait_for_selector("#consoleView:not([hidden])")
            page.click(".con-rail-item[data-console-panel='account']")
            page.wait_for_timeout(1200)
            rows = page.evaluate("() => [...document.querySelectorAll('#accountRows tr')].map(r => r.children[0].textContent)")
            check("管理员账号页列出 admin 与 alice", "admin" in rows and "alice" in rows, json.dumps(rows))
            page.click("#inviteCreate")
            page.wait_for_selector("#inviteFresh:not([hidden])", timeout=10000)
            fresh = page.text_content("#inviteFresh")
            check("管理员生成邀请码显示明文", len((fresh or "").strip()) == 8, fresh)
            page.screenshot(path=str(OUT / "ui-admin-account.png"))
            browser.close()
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
