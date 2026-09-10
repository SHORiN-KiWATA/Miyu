#!/usr/bin/env python3
"""WebUI 五项修复的 A/B 走查:沙箱 daemon + OpenAI 桩 + Playwright(Chromium)。

    BIN=<miyu 二进制> TAG=old|new python3 webui_verify.py

判定项:
  ctx_after_reload   会话钉了 stub-b(50k)后,刷新页面上下文条仍显示 50k(不是全局 stub-a 的 100k)
  ctx_after_settings 打开设置页(拉 /api/config)后仍是 50k
  enter_newline      手机触屏视口下回车插入换行、不发送
  ctrl_enter_sends   Ctrl+Enter 仍发送
  usage_stacked      手机视口用量页:环形图在表格上方,不并排
  jump_offset_synced 后台任务条撑高 dock 后,「回到底部」按钮的 bottom 跟着更新
  autoscroll_kept    第一轮结束触发整段重建时第二轮仍在流式,结束时视口仍贴底且按钮隐藏
产物:~/.cache/miyu-arch-fixes/webui-<TAG>/{report.json,daemon.log,*.png}
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

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "webui-fixes"))
import authlib  # noqa: E402

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"])
TAG = os.environ.get("TAG", "run")
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-arch-fixes")).expanduser() / f"webui-{TAG}"
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18481"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18499"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-a"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-a", "stub-b"],
            "model_context_window": {"stub-a": 100000, "stub-b": 50000},
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
    with authlib.OPENER.open(req, timeout=30) as resp:
        raw = resp.read()
    return json.loads(raw) if raw else None


def turn_count(session_id):
    turns = api("GET", f"/api/sessions/{session_id}/turns")
    if isinstance(turns, dict):
        turns = turns.get("turns", [])
    return len(turns or [])


def wait_idle(page, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        busy = page.evaluate("() => !!document.querySelector('[data-turn-status]') || !document.getElementById('sendButton') || document.getElementById('composerInput').disabled")
        running = api("GET", "/api/bootstrap").get("runs") or []
        if not busy and not running:
            return True
        time.sleep(0.4)
    return False


def main():
    if HOME.exists():
        shutil.rmtree(HOME)
    RUNTIME.mkdir(parents=True, exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    write_config()
    report = {"tag": TAG, "bin": str(BIN)}
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="job", STUB_CHUNK_SLEEP="0.04", STUB_LINES="70")
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_reasoning.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/health"), "daemon not up"
        authlib.bootstrap(BASE)
        time.sleep(1)

        created = api("POST", "/api/sessions", {"name": "走查", "switch": True})
        session_id = created.get("session_id") or created.get("id") or (created.get("session") or {}).get("session_id")
        assert session_id, f"no session id in {created}"
        api("PUT", f"/api/sessions/{session_id}/models", {"models": [{"provider_id": "stub", "model": "stub-b"}]})

        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            # ── 桌面:上下文窗口 + 自动滚动 + 按钮偏移 ──
            page = browser.new_page(viewport={"width": 1280, "height": 860})
            page.goto(BASE)
            authlib.ui_login(page)
            page.wait_for_selector("#contextNumbers")
            page.wait_for_timeout(1500)
            ctx1 = page.text_content("#contextNumbers")
            page.reload()
            page.wait_for_selector("#contextNumbers")
            page.wait_for_timeout(1500)
            ctx2 = page.text_content("#contextNumbers")
            report["ctx_text_initial"] = ctx1
            report["ctx_text_after_reload"] = ctx2
            report["ctx_after_reload"] = "50" in (ctx2 or "")
            # 打开设置页:拉一次 /api/config
            page.evaluate("() => fetch('/api/config').then(r => r.json())")
            opened = False
            for sel in ["#settingsButton", "[data-console='settings']", "button[title*='设置']", "#openSettings"]:
                if page.query_selector(sel):
                    page.click(sel)
                    opened = True
                    break
            page.wait_for_timeout(1200)
            ctx3 = page.text_content("#contextNumbers")
            report["settings_opened"] = opened
            report["ctx_text_after_settings"] = ctx3
            report["ctx_after_settings"] = "50" in (ctx3 or "")
            if opened:
                page.keyboard.press("Escape")
                page.goto(BASE)
                authlib.ui_login(page)
                page.wait_for_selector("#composerInput")
                page.wait_for_timeout(800)

            # 会话内换模型(仅本会话):stub-b(50k)→ stub-a(100k),上下文条应立刻变 100k
            page.click("#modelButton")
            page.wait_for_selector(".model-menu-item")
            items = page.query_selector_all(".model-menu-item")
            for item in items:
                text = item.inner_text()
                if "stub-a" in text or "stub-b" in text:
                    item.click()  # 勾上 a、取消 b
                    page.wait_for_timeout(150)
            page.click(".model-confirm")
            page.wait_for_timeout(1800)
            ctx4 = page.text_content("#contextNumbers")
            report["ctx_text_after_model_switch"] = ctx4
            report["ctx_after_model_switch"] = "100" in (ctx4 or "")
            api("PUT", f"/api/sessions/{session_id}/models", {"models": [{"provider_id": "stub", "model": "stub-b"}]})
            page.reload()
            page.wait_for_selector("#composerInput")
            page.wait_for_timeout(1200)

            # 按钮偏移:人为撑高 dock(模拟后台任务条出现),看内联 bottom 是否跟上
            page.wait_for_selector("#composerInput")
            page.evaluate("""() => {
                const strip = document.getElementById('jobsStrip');
                strip.hidden = false;
                strip.innerHTML = '<div style="height:96px">模拟后台任务条</div>';
            }""")
            page.wait_for_timeout(400)
            offset = page.evaluate("""() => ({
                dock: document.getElementById('composerDock').offsetHeight,
                bottom: parseFloat(document.getElementById('jumpBottomButton').style.bottom || '0'),
            })""")
            report["jump_offset"] = offset
            report["jump_offset_synced"] = abs(offset["bottom"] - (offset["dock"] + 10)) <= 1
            page.evaluate("() => { const s = document.getElementById('jobsStrip'); s.hidden = true; s.innerHTML = ''; }")
            page.wait_for_timeout(300)

            # 自动滚动:让模型起一个后台任务(sleep 4),任务完成后 daemon 自己起
            # 唤醒回合流长回复;唤醒回合一落盘就整段重建,重建落在流式中间。
            page.fill("#composerInput", "起一个后台任务")
            page.keyboard.press("Enter")
            if os.environ.get("FLAKY_SSE", "1") == "1":
                # 手机息屏/网络抖一下:SSE 断掉两秒再回来。唤醒回合在这期间起跑,
                # 页面靠轮询与 resync 把正在流式的回合当持久化回合整段重建。
                page.wait_for_timeout(1500)
                cdp = page.context.new_cdp_session(page)
                cdp.send("Network.enable")
                cdp.send("Network.emulateNetworkConditions", {"offline": True, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
                page.wait_for_timeout(2500)
                cdp.send("Network.emulateNetworkConditions", {"offline": False, "latency": 0, "downloadThroughput": -1, "uploadThroughput": -1})
            wait_idle(page, timeout=60)
            # 等唤醒回合出现并跑完
            deadline = time.time() + 90
            seen_wake = False
            while time.time() < deadline:
                runs = api("GET", "/api/bootstrap").get("runs") or []
                has_notice = page.evaluate("() => !!document.querySelector('.system-event')")
                if runs:
                    seen_wake = True
                if seen_wake and not runs and has_notice:
                    break
                time.sleep(0.5)
            page.wait_for_timeout(1200)
            scroll = page.evaluate("""() => {
                const el = document.getElementById('chatScroll');
                return { gap: el.scrollHeight - el.scrollTop - el.clientHeight,
                         jumpHidden: document.getElementById('jumpBottomButton').hidden,
                         notice: !!document.querySelector('.system-event'),
                         articles: document.querySelectorAll('#timeline article').length };
            }""")
            report["autoscroll"] = scroll
            report["autoscroll_kept"] = scroll["notice"] and scroll["gap"] <= 2 and scroll["jumpHidden"]
            page.screenshot(path=str(OUT / "desktop-after-wake.png"))
            page.close()

            # ── 手机触屏:回车换行 + 用量页竖排 ──
            ctx = browser.new_context(viewport={"width": 390, "height": 844}, is_mobile=True, has_touch=True,
                                      device_scale_factor=2)
            mp = ctx.new_page()
            mp.goto(BASE)
            authlib.ui_login(mp)
            mp.wait_for_selector("#composerInput")
            mp.wait_for_timeout(1200)
            before = turn_count(session_id)
            mp.click("#composerInput")
            mp.keyboard.type("第一行")
            mp.keyboard.press("Enter")
            mp.keyboard.type("第二行")
            mp.wait_for_timeout(500)
            value = mp.input_value("#composerInput")
            report["enter_value"] = value
            report["enter_newline"] = ("\n" in value) and turn_count(session_id) == before
            mp.screenshot(path=str(OUT / "mobile-composer.png"))
            mp.keyboard.press("Control+Enter")
            mp.wait_for_timeout(1500)
            report["ctrl_enter_sends"] = turn_count(session_id) == before + 1 or bool(api("GET", "/api/bootstrap").get("runs"))
            wait_idle(mp, timeout=90)

            # 只差 hash 的导航不重跑脚本,先离开再进(深链走启动路径)
            mp.goto("about:blank")
            mp.goto(BASE + "#console/usage")
            authlib.ui_login(mp)
            mp.wait_for_timeout(3000)
            layout = mp.evaluate("""() => {
                const body = document.querySelector('.u-model-body');
                if (!body) return null;
                const d = body.querySelector('.u-donut-wrap')?.getBoundingClientRect();
                const t = body.querySelector('.u-table-scroll')?.getBoundingClientRect();
                if (!d || !t) return { body: true, donut: !!d, table: !!t };
                return { donutBottom: d.bottom, tableTop: t.top, donutLeft: d.left, donutRight: d.right,
                         tableLeft: t.left, tableRight: t.right, vw: window.innerWidth };
            }""")
            report["usage_layout"] = layout
            report["usage_stacked"] = bool(layout and layout.get("tableTop") is not None
                                            and layout["tableTop"] >= layout["donutBottom"] - 1)
            mp.screenshot(path=str(OUT / "mobile-usage.png"), full_page=True)
            ctx.close()
            browser.close()
    finally:
        for p in (daemon, stub):
            if p:
                p.terminate()
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    p.kill()
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    keys = ["ctx_after_reload", "ctx_after_settings", "ctx_after_model_switch", "enter_newline", "ctrl_enter_sends", "usage_stacked", "jump_offset_synced", "autoscroll_kept"]
    for k in keys:
        print(f"{TAG:4} {k:20} {report.get(k)}")
    print("details:", json.dumps({k: report.get(k) for k in ["ctx_text_initial", "ctx_text_after_reload", "ctx_text_after_settings", "ctx_text_after_model_switch", "jump_offset", "autoscroll", "usage_layout", "enter_value"]}, ensure_ascii=False))


if __name__ == "__main__":
    sys.exit(main())
