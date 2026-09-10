#!/usr/bin/env python3
"""打字抖动取证:在输入框逐字敲,每敲一个字采一帧(滚动位置、时间线/输入框的几何、
累计 layout-shift),两份前端(WEB 目录)对着同一个 daemon 跑,A/B 看谁在抖。

    # 对着已有 daemon(比如沙盒)
    BASE=http://127.0.0.1:8388 PASSWORD=miyu-sandbox WEB=<web 目录> TAG=x python3 testkit/multi-user/typing_probe.py
    # 自己起桩模型 + 隔离 daemon,先跑一轮长输出,流式中打一遍、结束后再打一遍
    BIN=<miyu> SPAWN=1 TURN=1 WEB=<web 目录> TAG=x python3 testkit/multi-user/typing_probe.py

页面用的 index.html/app.js/styles.css 从 WEB 目录拦截替换(同 testkit/webui-timeline/run.py)。
输出 ~/.cache/miyu-typing-probe/<tag>/{samples.json,typing-*.png,video/}。
"""
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
import e2e  # noqa: E402

WEB = Path(os.environ.get("WEB", REPO / "web")).resolve()
TAG = os.environ.get("TAG", WEB.parent.name)
OUT = Path("~/.cache/miyu-typing-probe").expanduser() / TAG
TEXT = os.environ.get("TEXT", "这是一段用来测试输入框抖动的文字,一边打一边看页面有没有整体在动。再来一行看看换行的时候会不会跳。")
MOBILE = os.environ.get("MOBILE") == "1"
SPAWN = os.environ.get("SPAWN") == "1"
TURN = os.environ.get("TURN") == "1"
PORT = int(os.environ.get("PORT", "18512"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18516"))
BASE = os.environ.get("BASE", f"http://127.0.0.1:{PORT}" if SPAWN else "http://127.0.0.1:8388")
PASSWORD = os.environ.get("PASSWORD", e2e.ADMIN_PASSWORD if SPAWN else "miyu-sandbox")
USERNAME = os.environ.get("USERNAME_", "admin" if SPAWN else "shorin")

SAMPLE_JS = """() => {
  const scroller = document.getElementById('chatScroll');
  const r = (el) => { if (!el) return null; const b = el.getBoundingClientRect(); return {top: Math.round(b.top), left: Math.round(b.left), w: Math.round(b.width), h: Math.round(b.height)}; };
  const last = [...document.querySelectorAll('.assistant-message')].pop();
  return {
    scrollTop: scroller ? scroller.scrollTop : null,
    scrollHeight: scroller ? scroller.scrollHeight : null,
    stage: r(document.getElementById('conversationStage')),
    timeline: r(document.getElementById('timeline')),
    last: r(last),
    dock: r(document.getElementById('composerDock')),
    input: r(document.getElementById('composerInput')),
    sidebar: r(document.getElementById('sidebar')),
    shift: window.__cls || 0,
    shifts: (window.__shiftLog || []).length,
    live: Boolean(last && last.classList.contains('live-assistant')),
  };
}"""

CLS_JS = """() => {
  window.__cls = 0; window.__shiftLog = [];
  try {
    new PerformanceObserver((list) => {
      for (const e of list.getEntries()) {
        if (e.hadRecentInput) continue;
        window.__cls += e.value;
        window.__shiftLog.push({t: Math.round(e.startTime), v: Math.round(e.value * 10000) / 10000, src: (e.sources || []).map((s) => (s.node && (s.node.id || s.node.className || s.node.tagName)) || '?').slice(0, 3)});
      }
    }).observe({type: 'layout-shift', buffered: true});
  } catch (error) { window.__clsError = String(error); }
}"""


def serve_local(route):
    url = route.request.url
    name = url.split("?")[0].rsplit("/", 1)[-1] or "index.html"
    local = WEB / name
    if local.exists():
        ctype = {"html": "text/html; charset=utf-8", "js": "application/javascript; charset=utf-8",
                 "css": "text/css; charset=utf-8"}[name.rsplit(".", 1)[-1]]
        route.fulfill(status=200, body=local.read_bytes(), headers={"content-type": ctype, "cache-control": "no-store"})
    else:
        route.continue_()


def type_and_sample(page, label, text):
    page.evaluate(CLS_JS)
    page.wait_for_timeout(200)
    page.click("#composerInput")
    samples = [dict(i=-1, ch="", **page.evaluate(SAMPLE_JS))]
    page.screenshot(path=str(OUT / f"{label}-000.png"))
    steps = list(enumerate(text)) if text else [(i, "") for i in range(49)]
    for i, ch in steps:
        if ch:
            page.keyboard.type(ch, delay=0)
        page.wait_for_timeout(60)
        samples.append(dict(i=i, ch=ch, **page.evaluate(SAMPLE_JS)))
        if i in (5, 20, 40, len(steps) - 1):
            page.screenshot(path=str(OUT / f"{label}-{i + 1:03d}.png"))
    shifts = page.evaluate("() => window.__shiftLog || []")
    page.fill("#composerInput", "")
    moved = {"scrollTop": 0, "stage": 0, "timeline": 0, "last": 0, "dock": 0, "sidebar": 0, "input_top": 0, "input_h": 0}
    for prev, cur in zip(samples, samples[1:]):
        if prev["scrollTop"] != cur["scrollTop"]:
            moved["scrollTop"] += 1
        for key in ("stage", "timeline", "last", "dock", "sidebar"):
            a, b = prev.get(key), cur.get(key)
            if a and b and (a["top"] != b["top"] or a["left"] != b["left"] or a["w"] != b["w"] or a["h"] != b["h"]):
                moved[key] += 1
        a, b = prev.get("input"), cur.get("input")
        if a and b:
            if a["top"] != b["top"]:
                moved["input_top"] += 1
            if a["h"] != b["h"]:
                moved["input_h"] += 1
    print(f"[{TAG}/{label}] keystrokes={len(steps)} cls={samples[-1]['shift']:.4f} shifts={len(shifts)} live={samples[-1]['live']} moved={json.dumps(moved)}")
    for s in shifts[:8]:
        print("  shift", s)
    return {"samples": samples, "shifts": shifts, "moved": moved}


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("*.png"):
        old.unlink()
    stub = daemon = None
    if SPAWN:
        e2e.PORT, e2e.STUB_PORT, e2e.BASE = PORT, STUB_PORT, BASE
        e2e.OUT = OUT / "daemon"
        e2e.HOME = e2e.OUT / "home"
        e2e.RUNTIME = e2e.OUT / "runtime"
        e2e.ENV = dict(os.environ, MIYU_HOME=str(e2e.HOME), XDG_RUNTIME_DIR=str(e2e.RUNTIME),
                       MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")
        import shutil
        if e2e.OUT.exists():
            shutil.rmtree(e2e.OUT)
        e2e.HOME.mkdir(parents=True)
        e2e.RUNTIME.mkdir(parents=True)
        e2e.write_config()
        stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), MODE="long", STUB_CHUNK_SLEEP="0.05", STUB_LINES="60")
        stub = subprocess.Popen([sys.executable, str(REPO / "testkit/webui-fixes/stub_reasoning.py")], env=stub_env,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        assert e2e.wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(e2e.BIN), "__daemon", "--port", str(PORT), "--bind", "127.0.0.1"],
                                  env=e2e.ENV, cwd=str(e2e.HOME), stdout=(e2e.OUT / "daemon.log").open("w"),
                                  stderr=subprocess.STDOUT)
        assert e2e.wait_http(f"{BASE}/api/health"), "daemon not up"
        time.sleep(1)
        e2e.bootstrap_admin(e2e.Client())
    results = {}
    try:
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            context = browser.new_context(
                viewport={"width": 390, "height": 844} if MOBILE else {"width": 1280, "height": 860},
                is_mobile=MOBILE, has_touch=MOBILE, device_scale_factor=2 if MOBILE else 1,
                record_video_dir=str(OUT / "video"))
            page = context.new_page()
            page.on("pageerror", lambda error: print("PAGEERROR", error))
            page.on("console", lambda msg: print("CONSOLE", msg.type, msg.text) if msg.type == "error" and "401" not in msg.text else None)
            page.route("**/index.html", serve_local)
            page.route("**/", serve_local)
            page.route("**/app.js*", serve_local)
            page.route("**/styles.css*", serve_local)
            page.goto(BASE)
            page.wait_for_selector("#loginForm:not([hidden])", timeout=15000)
            if USERNAME:
                page.fill("#loginUsername", USERNAME)
            page.fill("#loginPassword", PASSWORD)
            page.click("#loginSubmit")
            page.wait_for_function("() => !document.body.classList.contains('is-blocked')", timeout=20000)
            page.wait_for_timeout(1200)
            if page.evaluate("() => { const o = document.getElementById('oobe'); return Boolean(o && !o.hidden); }"):
                page.click("#oobeSkip")
                page.wait_for_timeout(600)
            page.wait_for_function("() => !document.getElementById('composerInput').disabled", timeout=20000)
            results["idle"] = type_and_sample(page, "idle", TEXT)
            if TURN:
                page.fill("#composerInput", "请用大约六十行介绍一下 Arch Linux")
                page.click("#sendButton")
                page.wait_for_selector(".assistant-message.live-assistant", timeout=20000)
                page.wait_for_timeout(400)
                # 对照:流式中不打字,只采样——看滚动/几何变化是内容增长还是打字引起的
                results["streaming-notype"] = type_and_sample(page, "streaming-notype", "")
                results["streaming"] = type_and_sample(page, "streaming", TEXT)
                page.wait_for_function("() => !document.querySelector('.assistant-message.live-assistant')", timeout=60000)
                page.wait_for_timeout(800)
                results["after"] = type_and_sample(page, "after", TEXT)
            context.close()
            browser.close()
    finally:
        for proc in (daemon, stub):
            if proc:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except Exception:
                    proc.kill()
    (OUT / "samples.json").write_text(json.dumps(results, ensure_ascii=False, indent=1))
    print("out:", OUT)


if __name__ == "__main__":
    main()
