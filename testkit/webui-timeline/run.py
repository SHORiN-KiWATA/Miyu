#!/usr/bin/env python3
"""过程时间线的真机走查:沙箱 daemon + 工具剧本桩(stub_tools.py)+ Playwright(Chromium)。

    BIN=<miyu 二进制> WEB=<web 目录> python3 testkit/webui-timeline/run.py

web/*.js 与 styles.css 编进二进制,所以 WEB 给的是哪个目录,页面就用哪份前端——
Playwright 拦下 index.html / app.js / styles.css 换成 WEB 里的文件,二进制本身不用重编。

判定项(一轮「思考 → 2 工具 → 说话 → 1 失败工具 → 思考 → 1 工具 → 最终回答」):
  live_groups        实时:说话把时间线切成两条,第一条 1 思考 2 工具,第二条 1 工具 + 1 思考 + 1 工具
  live_head_hidden   实时运行中的那条没有总结行
  live_collapsed     她一开口,前一条收起、总结行出现,文字形如「Worked for 1.2 s · 2 tools · 1 thought」
  err_marked         失败的那条工具签 is-failure,所在组总结行含「1 err」
  rail_sized         切断后每条时间线的细线有高度(展开态)或缩到 0(收起态)
  persisted_groups   刷新后从 turn.tool_flow 重建,分组数与实时一致,总结行没有耗时
  no_times           用户消息和助手名字旁都没有时间
  toggle_off_on      设置里关掉「过程自动收起」→ 总结行藏起、全部展开;再开 → 收回
  console_clean      全程无 pageerror / console.error
产物:~/.cache/miyu-webui-timeline/{report.json,daemon.log,*.png}
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

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"]).expanduser()
WEB = Path(os.environ.get("WEB", HERE.parent.parent / "web")).resolve()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-webui-timeline")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18483"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18497"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-tools"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-tools"],
            "model_context_window": {"stub-tools": 100000},
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


def api(method, path, payload=None):
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else {}


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


GROUPS_JS = """() => {
  const last = [...document.querySelectorAll('.assistant-message')].pop();
  if (!last) return null;
  return {
    live: last.classList.contains('live-assistant'),
    lines: [...last.querySelectorAll('.proc-line')].map((l) => ({
      open: l.classList.contains('is-open'), live: l.classList.contains('is-live'), static: l.classList.contains('is-static'),
      headHidden: l.querySelector('.proc-head').hidden, summary: l.querySelector('.proc-summary')?.textContent || '',
      tools: l.querySelectorAll('.tool-card').length, thoughts: l.querySelectorAll('.reasoning-block').length,
      failures: l.querySelectorAll('.tool-card.is-failure').length,
      railTop: parseFloat(l.querySelector('.proc-rail').style.top || '0'), railHeight: parseFloat(l.querySelector('.proc-rail').style.height || '0'),
    })),
    texts: [...last.querySelectorAll(':scope .assistant-blocks > .markdown-body')].map((m) => m.textContent.slice(0, 40)),
    labelSpans: [...last.querySelectorAll('.assistant-label span')].map((s) => s.textContent),
    userActionSpans: [...document.querySelectorAll('.user-message .message-actions > span')].map((s) => s.textContent),
  };
}"""


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    report = {"bin": str(BIN), "web": str(WEB)}
    errors = []
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_tools.py")], env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon not up"
        time.sleep(1)
        created = api("POST", "/api/sessions", {"name": "时间线走查", "switch": True})
        session_id = created.get("session_id") or created.get("id") or (created.get("session") or {}).get("session_id")
        assert session_id, f"no session id in {created}"

        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 900})
            page.on("pageerror", lambda e: errors.append(f"pageerror: {e}"))
            # 沙箱 home 没有 matugen 主题文件和人格头像,那两处 404 是既有噪声,不算错
            page.on("console", lambda m: errors.append(f"console: {m.text}") if m.type == "error" and "status of 404" not in m.text else None)
            page.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            page.goto(BASE)
            page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
            page.wait_for_timeout(800)
            page.fill("#composerInput", "跑一下时间线剧本")
            page.click("#sendButton")

            # 实时:轮询,抓「第一条切断、第二条运行中」那一刻
            t0 = time.time()
            live_snapshot = None
            shots = 0
            while time.time() - t0 < 60:
                info = page.evaluate(GROUPS_JS)
                if info and info["live"] and shots == 0 and info["lines"] and info["lines"][0]["tools"] >= 1:
                    page.screenshot(path=str(OUT / "01-live-first-group.png")); shots = 1
                if info and info["live"] and len(info["lines"]) >= 2 and info["lines"][1]["tools"] >= 1 and shots == 1:
                    page.wait_for_timeout(500)
                    live_snapshot = page.evaluate(GROUPS_JS)
                    page.screenshot(path=str(OUT / "02-live-second-group.png")); shots = 2
                if info and not info["live"] and info["texts"] and time.time() - t0 > 3:
                    break
                page.wait_for_timeout(250)
            page.wait_for_timeout(1200)
            page.screenshot(path=str(OUT / "03-done.png"))
            done = page.evaluate(GROUPS_JS)
            report["live_snapshot"] = live_snapshot
            report["done"] = done
            ls = (live_snapshot or {}).get("lines") or []
            report["live_head_hidden"] = bool(ls) and ls[-1]["live"] and ls[-1]["headHidden"]
            report["live_collapsed"] = bool(ls) and (not ls[0]["open"]) and (not ls[0]["headHidden"]) and ls[0]["summary"].startswith("Worked for") and "2 tools" in ls[0]["summary"] and "1 thought" in ls[0]["summary"]
            dl = (done or {}).get("lines") or []
            report["live_groups"] = len(dl) == 2 and dl[0]["tools"] == 2 and dl[0]["thoughts"] == 1 and dl[1]["tools"] == 2 and dl[1]["thoughts"] == 1
            report["err_marked"] = len(dl) == 2 and dl[1]["failures"] == 1 and "1 err" in dl[1]["summary"]
            report["rail_sized"] = all((not l["open"] and l["railHeight"] == 0) or (l["open"] and l["railHeight"] > 20) for l in dl)
            report["no_times"] = done is not None and not any(s.strip() for s in done["labelSpans"]) and not done["userActionSpans"]

            # 展开第二条 + 点开失败那行的详情
            page.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message')].pop().querySelectorAll('.proc-line')[1]; l.querySelector('.proc-head').click(); }")
            page.wait_for_timeout(600)
            page.evaluate("() => { const last = [...document.querySelectorAll('.assistant-message')].pop(); (last.querySelector('.tool-card.is-failure .tool-head') || last.querySelector('.proc-line:last-of-type .tool-card .tool-head'))?.click(); }")
            page.wait_for_timeout(600)
            page.screenshot(path=str(OUT / "04-expanded.png"))
            report["rail_after_expand"] = page.evaluate(GROUPS_JS)["lines"][1]["railHeight"]

            # 刷新:回看那份
            page.reload()
            page.wait_for_selector("#composerInput", timeout=20000)
            page.wait_for_timeout(2000)
            page.screenshot(path=str(OUT / "05-reloaded.png"))
            again = page.evaluate(GROUPS_JS)
            report["reloaded"] = again
            al = (again or {}).get("lines") or []
            report["persisted_groups"] = len(al) == 2 and [(l["tools"], l["thoughts"]) for l in al] == [(2, 1), (2, 1)] and all(l["static"] and not l["headHidden"] and not l["open"] for l in al) and all("Worked" not in l["summary"] and l["summary"].startswith("2 tools") for l in al)
            report["persisted_err"] = len(al) == 2 and al[1]["failures"] == 1 and "1 err" in al[1]["summary"]
            # 亮色主题也看一眼(用户日常用亮色)
            page.click("#sidebarThemeButton")
            page.wait_for_timeout(500)
            page.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message')].pop().querySelectorAll('.proc-line')[1]; l.querySelector('.proc-head').click(); }")
            page.wait_for_timeout(700)
            page.evaluate("() => { const last = [...document.querySelectorAll('.assistant-message')].pop(); last.querySelector('.tool-card.is-failure .tool-head')?.click(); }")
            page.wait_for_timeout(600)
            page.screenshot(path=str(OUT / "05b-reloaded-light.png"))
            page.click("#sidebarThemeButton")
            page.wait_for_timeout(400)

            # 设置开关
            page.click("#sidebarSettingsButton")
            page.wait_for_selector("#procCollapseToggle", timeout=10000)
            page.wait_for_timeout(600)
            page.click("#procCollapseToggle")
            page.wait_for_timeout(600)
            off = page.evaluate("[...document.querySelectorAll('.proc-line')].map(l => [l.classList.contains('is-open'), l.querySelector('.proc-head').hidden])")
            page.click("#procCollapseToggle")
            page.wait_for_timeout(600)
            on = page.evaluate("[...document.querySelectorAll('.proc-line')].map(l => [l.classList.contains('is-open'), l.querySelector('.proc-head').hidden])")
            report["toggle_off"] = off
            report["toggle_on"] = on
            report["toggle_off_on"] = all(o == [True, True] for o in off) and all(o == [False, False] for o in on)
            report["prefs"] = api("GET", "/api/ui-prefs")
            page.keyboard.press("Escape")
            page.wait_for_timeout(400)
            # 手机视口再看一眼
            phone = browser.new_page(viewport={"width": 390, "height": 844}, device_scale_factor=2, is_mobile=True, has_touch=True)
            phone.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            phone.goto(BASE)
            phone.wait_for_selector("#composerInput", timeout=20000)
            phone.wait_for_timeout(2000)
            phone.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message')].pop().querySelectorAll('.proc-line')[1]; l?.querySelector('.proc-head')?.click(); }")
            phone.wait_for_timeout(700)
            phone.screenshot(path=str(OUT / "06-phone.png"))
            browser.close()
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        stub.terminate()
    report["errors"] = errors
    report["console_clean"] = not errors
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), "utf-8")
    keys = ["live_groups", "live_head_hidden", "live_collapsed", "err_marked", "rail_sized", "persisted_groups", "persisted_err", "no_times", "toggle_off_on", "console_clean"]
    for k in keys:
        print(f"{'  ok ' if report.get(k) else 'FAIL '} {k}")
    print("report:", OUT / "report.json")
    return 0 if all(report.get(k) for k in keys) else 1


if __name__ == "__main__":
    sys.exit(main())
