#!/usr/bin/env python3
"""「那 3.4MB 值不值」的对比实验:同一张流程图,ECharts 版 vs Mermaid 版,各截一张图。

    BIN=<miyu 二进制> MERMAID=/path/to/mermaid.min.js python3 testkit/webui-artifact/compare_mermaid.py

Mermaid **没有**进仓库——这里由 Playwright 在路由层临时喂进去(带上 vendor 那组
放行头),所以不用为一次比较去改后端、重编译。要是最后决定加,再照 ECharts 那套
落进 web/vendor/ 即可。

产出:~/.cache/miyu-mermaid-compare/{10-echarts-flow.png,11-mermaid-flow.png,12-mermaid-seq.png}
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
MERMAID = Path(os.environ.get("MERMAID", "/tmp/mermaid-try/mermaid.min.js")).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-mermaid-compare")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18487"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18494"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-artifact"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-artifact"],
            "model_context_window": {"stub-artifact": 100000},
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
    request = urllib.request.Request(
        BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=10) as response:
        raw = response.read()
        return json.loads(raw) if raw else {}


def serve_local(route):
    name = route.request.url.split("?")[0].rsplit("/", 1)[-1] or "index.html"
    local = WEB / name
    if local.exists():
        ctype = {"html": "text/html; charset=utf-8", "js": "application/javascript; charset=utf-8",
                 "css": "text/css; charset=utf-8"}[name.rsplit(".", 1)[-1]]
        route.fulfill(status=200, body=local.read_bytes(),
                      headers={"content-type": ctype, "cache-control": "no-store"})
    else:
        route.continue_()


def serve_mermaid(route):
    """仓库里没有 mermaid,这里现喂。头照 assets.rs 的 allow_sandboxed_frames 抄,
    否则不透明源的 iframe 会被 Private Network Access 拦掉。"""
    if route.request.method == "OPTIONS":
        route.fulfill(status=204, headers={
            "access-control-allow-origin": "*",
            "access-control-allow-private-network": "true",
            "access-control-allow-methods": "GET, OPTIONS",
        })
        return
    route.fulfill(status=200, body=MERMAID.read_bytes(), headers={
        "content-type": "text/javascript; charset=utf-8",
        "cache-control": "no-store",
        "access-control-allow-origin": "*",
        "access-control-allow-private-network": "true",
    })


def pick(page, name):
    page.click("#artifactTitleButton")
    page.wait_for_timeout(250)
    rows = page.locator(".artifact-resource-menu button[role='menuitem']")
    for index in range(rows.count()):
        if name in rows.nth(index).inner_text():
            rows.nth(index).click()
            page.wait_for_timeout(600)
            return True
    page.keyboard.press("Escape")
    return False


def main():
    assert MERMAID.exists(), f"没找到 mermaid：{MERMAID}"
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    report = {"mermaid_bytes": MERMAID.stat().st_size}
    console = []
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_compare.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "桩没起来"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon 没起来"
        time.sleep(1)
        api("POST", "/api/sessions", {"name": "mermaid 对比", "switch": True})

        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1440, "height": 900})
            page.on("console", lambda m: console.append(f"{m.type}: {m.text}")
                    if m.type == "error" and "404" not in m.text else None)
            page.route(lambda u: "/vendor/mermaid/" in u, serve_mermaid)
            page.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(
                k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            page.goto(BASE)
            page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
            page.wait_for_timeout(600)
            page.fill("#composerInput", "画三张图")
            page.click("#sendButton")
            deadline = time.time() + 60
            while time.time() < deadline:
                if page.evaluate("Boolean(document.querySelector('#artifactToggleButton:not([hidden])'))"):
                    break
                page.wait_for_timeout(300)
            page.wait_for_timeout(2500)
            if page.evaluate("document.getElementById('artifactWorkspace').hidden"):
                page.click("#artifactToggleButton")
                page.wait_for_timeout(600)
            # 图占面板大半,拉宽一点看得清
            page.evaluate("() => { document.getElementById('artifactMaximizeButton').click(); }")
            page.wait_for_timeout(500)

            for key, name, shot in (("echarts_flow", "flow-echarts.html", "10-echarts-flow.png"),
                                    ("mermaid_flow", "flow-mermaid.html", "11-mermaid-flow.png"),
                                    ("mermaid_seq", "seq-mermaid.html", "12-mermaid-seq.png")):
                if not pick(page, name):
                    report[key] = "没找到这份文件"
                    continue
                page.wait_for_timeout(3500)
                frame = page.frame_locator("#artifactView iframe")
                report[key] = {
                    "svg": frame.locator("svg").count(),
                    "canvas": frame.locator("canvas").count(),
                }
                page.screenshot(path=str(OUT / shot))
            browser.close()
    finally:
        for process in (daemon, stub):
            if process:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except Exception:
                    process.kill()
    report["console"] = console[:20]
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), "utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"截图在 {OUT}")


if __name__ == "__main__":
    main()
