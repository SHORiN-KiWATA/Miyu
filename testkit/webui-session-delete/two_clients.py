#!/usr/bin/env python3
"""复现:WebUI 删掉最后一个可见会话后冒出几个新会话。

    BIN=target/release/miyu python3 delete_last.py
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

BIN = Path(os.environ["BIN"])
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-delete-last")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18493"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-a"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": "http://127.0.0.1:1/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-a"],
        }],
        "memory": {"enabled": False},
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
    with urllib.request.urlopen(req, timeout=30) as resp:
        raw = resp.read()
    return json.loads(raw) if raw else None


def visible_sessions():
    payload = api("GET", "/api/sessions")
    rows = payload.get("sessions") if isinstance(payload, dict) else payload
    return [r for r in rows or [] if r.get("session_id") != "default"]


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    RUNTIME.mkdir(parents=True, exist_ok=True)
    write_config()
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    try:
        assert wait_http(f"{BASE}/api/config"), "daemon not up"
        time.sleep(1)
        created = api("POST", "/api/sessions", {"name": "唯一会话"})
        sid = (created.get("session") or {}).get("session_id") or created.get("session_id")
        print("before:", [(r["session_id"], r["name"]) for r in visible_sessions()])
        posts = []
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 860})
            page.on("request", lambda r: posts.append((r.method, r.url)) if r.method in ("POST", "DELETE") and "/api/sessions" in r.url else None)
            page.on("dialog", lambda d: d.accept())
            page.goto(BASE)
            page.wait_for_selector(f'.session-item[data-session-id="{sid}"]')
            page.click(f'.session-item[data-session-id="{sid}"] .session-item-main')
            page.wait_for_timeout(800)
            page2 = browser.new_page(viewport={"width": 390, "height": 800})
            page2.on("request", lambda r: posts.append(("P2", r.method, r.url)) if r.method in ("POST", "DELETE") and "/api/sessions" in r.url else None)
            page2.goto(BASE)
            page2.wait_for_selector(f'.session-item[data-session-id="{sid}"]', state="attached")
            page2.wait_for_timeout(800)
            print("page2 view:", page2.evaluate("() => document.querySelector('.session-item.active')?.dataset.sessionId"))
            page.click(f'.session-item[data-session-id="{sid}"] .session-menu-button')
            page.wait_for_selector('.session-menu button.is-danger')
            page.click('.session-menu button.is-danger')
            page.wait_for_timeout(3000)
            page.screenshot(path=str(OUT / "after.png"))
            sidebar = page.evaluate("() => [...document.querySelectorAll('.session-item')].map(e => e.dataset.sessionId)")
            browser.close()
        after = visible_sessions()
        print("requests:", posts)
        print("sidebar:", sidebar)
        print("after:", [(r["session_id"], r["name"]) for r in after])
        print("RESULT new sessions =", len(after))
    finally:
        daemon.terminate()
        try:
            daemon.wait(5)
        except Exception:
            daemon.kill()


if __name__ == "__main__":
    main()
