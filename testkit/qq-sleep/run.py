#!/usr/bin/env python3
"""QQ 睡眠时间黑盒:沙箱 daemon + OpenAI 桩 + 假 NapCat(反向 WS)。

    BIN=<miyu> python3 sleep_e2e.py

判定:
  睡眠中  游客群 @ / 游客私聊 / 白名单群 @ → 不回;白名单私聊 / 管理员私聊 / 管理员群 @ → 回
  校验    PUT 坏格式 → 400;PUT 清空 → 200
  醒来后  游客群 @ → 回
  设置页  QQ 页有「睡眠时间」输入框
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta
from pathlib import Path

REPO = Path("/home/shorin/Documents/github/Miyu")
BIN = Path(os.environ["BIN"])
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-sleep-e2e")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18523"))
QQ_PORT = int(os.environ.get("QQ_PORT", "18524"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18525"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))

spec = importlib.util.spec_from_file_location("fake", REPO / "testkit" / "fake-onebot" / "run.py")
fake = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fake)
fake.PORT = QQ_PORT
ADMIN, WHITE, GUEST = 1, 2, 3


def window_around_now(hours=1):
    now = datetime.now()
    start = (now - timedelta(hours=hours)).strftime("%H:%M")
    end = (now + timedelta(hours=hours)).strftime("%H:%M")
    return f"{start}-{end}"


def write_config(sleep_hours):
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-a"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-a"],
        }],
        "memory": {"enabled": False},
        "platforms": {"qq": {
            "enabled": True, "reverse_ws_port": QQ_PORT, "access_token": "",
            "admin_users": [ADMIN], "private_chats": {"whitelist": [WHITE]},
            "sleep_hours": sleep_hours,
        }},
    }
    (HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


def api(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method,
                                 headers={"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode(errors="replace")


CALLS = []


def pump(ws):
    while True:
        try:
            frame = ws.recv()
        except Exception as error:
            print(f"  [pump] end: {error}")
            return
        if not isinstance(frame, dict) or "action" not in frame:
            continue
        action, params = frame["action"], frame.get("params", {})
        if action in ("send_group_msg", "send_msg", "send_private_msg"):
            CALLS.append((action, params.get("group_id"), params.get("user_id"), fake.render(params.get("message"))))
            print(f"  ← {action} group={params.get('group_id')} user={params.get('user_id')}: {fake.render(params.get('message'))[:40]}")
        ws.send({"status": "ok", "retcode": 0, "data": fake.api_data(action, params), "echo": frame.get("echo")})


def expect(ws, label, send, should_reply, wait=6.0):
    before = len(CALLS)
    send()
    deadline = time.time() + wait
    while time.time() < deadline and len(CALLS) == before:
        time.sleep(0.2)
    replied = len(CALLS) > before
    ok = replied == should_reply
    print(f"{'✓' if ok else '✗'} {label}: {'回了' if replied else '没回'}(期望{'回' if should_reply else '不回'})")
    if replied and not should_reply:
        pass
    # 让可能迟到的回复也落进来,避免串到下一项
    time.sleep(1.5 if should_reply else 0)
    return ok


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    RUNTIME.mkdir(parents=True, exist_ok=True)
    sleep_hours = window_around_now()
    write_config(sleep_hours)
    print("sleep_hours =", sleep_hours)
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="plain", STUB_CHUNK_SLEEP="0.01")
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit" / "webui-fixes" / "stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    results = {}
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon not up"
        time.sleep(1.5)
        ws = fake.WS.connect("")
        threading.Thread(target=pump, args=(ws,), daemon=True).start()
        time.sleep(1.0)

        g = lambda text, sender, at=True: (lambda: fake.group_msg(ws, text, sender=sender, at_self=at))
        p = lambda text, sender: (lambda: fake.private_msg(ws, text, sender=sender))
        results["sleep_guest_group_at"] = expect(ws, "睡眠·游客群@", g("在吗", GUEST), False)
        results["sleep_guest_private"] = expect(ws, "睡眠·游客私聊", p("在吗", GUEST), False)
        results["sleep_white_group_at"] = expect(ws, "睡眠·白名单群@", g("在吗", WHITE), False)
        results["sleep_white_private"] = expect(ws, "睡眠·白名单私聊", p("在吗", WHITE), True, wait=20)
        results["sleep_admin_private"] = expect(ws, "睡眠·管理员私聊", p("在吗", ADMIN), True, wait=20)
        results["sleep_admin_group_at"] = expect(ws, "睡眠·管理员群@", g("在吗", ADMIN), True, wait=20)

        status, cfg = api("GET", "/api/config")
        assert status == 200, status
        assert cfg["config"]["platforms"]["qq"]["sleep_hours"] == sleep_hours, cfg["config"]["platforms"]["qq"]
        bad = json.loads(json.dumps(cfg["config"]))
        bad["platforms"]["qq"]["sleep_hours"] = "night"
        status, body = api("PUT", "/api/config", {"config": bad, "prompts": cfg["prompts"]})
        results["put_bad_rejected"] = status == 400
        print(f"{'✓' if status == 400 else '✗'} PUT 坏格式 → {status} {str(body)[:100]}")
        good = json.loads(json.dumps(cfg["config"]))
        good["platforms"]["qq"]["sleep_hours"] = ""
        status, body = api("PUT", "/api/config", {"config": good, "prompts": cfg["prompts"]})
        results["put_clear_ok"] = status == 200
        print(f"{'✓' if status == 200 else '✗'} PUT 清空 → {status}")
        time.sleep(1.5)
        results["awake_guest_group_at"] = expect(ws, "醒来·游客群@", g("在吗", GUEST), True, wait=20)

        from playwright.sync_api import sync_playwright
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 900})
            page.goto(BASE, wait_until="networkidle")
            page.click("#sidebarSettingsButton")
            page.wait_for_selector('[data-console-panel="settings"]:not([hidden])')
            page.wait_for_function("() => document.getElementById('settingsStatus')?.textContent?.includes('配置已同步')", timeout=15000)
            page.click('[data-settings-view="qq"]')
            page.wait_for_timeout(300)
            field = page.query_selector('#settings-qq input[aria-label="睡眠时间"]')
            results["settings_field_present"] = field is not None
            if field:
                field.scroll_into_view_if_needed()
                page.wait_for_timeout(200)
                field.fill("23:00-07:00")
                page.wait_for_timeout(300)
                page.screenshot(path=str(OUT / "settings-qq-sleep.png"))
                # 保存:找保存按钮
                save = page.query_selector("#saveConfigButton")
                if save:
                    save.click()
                    page.wait_for_timeout(1500)
                    status, cfg2 = api("GET", "/api/config")
                    results["settings_saved"] = cfg2["config"]["platforms"]["qq"].get("sleep_hours") == "23:00-07:00"
            print(f"{'✓' if results.get('settings_field_present') else '✗'} 设置页字段存在; saved={results.get('settings_saved')}")
            browser.close()
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(5)
            except Exception:
                daemon.kill()
        stub.terminate()
    print("RESULTS", json.dumps(results, ensure_ascii=False))
    print("ALL_OK" if all(results.values()) else "SOME_FAILED")


if __name__ == "__main__":
    main()
