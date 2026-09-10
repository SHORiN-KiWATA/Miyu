#!/usr/bin/env python3
"""把任意几份 HTML 当成真 artifact 塞进面板渲染并截图。

    BIN=<miyu 二进制> FILES=a.html,b.html python3 testkit/webui-artifact/render_html.py

用来验「她写出来的东西到底跑不跑得起来」——直接用浏览器开 file:// 不算数,
那样既没有 artifact 的 CSP,也取不到 /vendor/ 的库。这里走的是完整真实路径:
沙箱 daemon → artifact 工具落盘 → 面板 iframe → 后端下发的 CSP。

产出:~/.cache/miyu-render-html/{<文件名>.png,report.json}
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"]).expanduser()
WEB = Path(os.environ.get("WEB", HERE.parent.parent / "web")).resolve()
FILES = [Path(p).expanduser() for p in os.environ["FILES"].split(",") if p.strip()]
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-render-html")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18491"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18492"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def patch_text():
    parts = ["*** Begin Patch"]
    for path in FILES:
        parts.append(f"*** Add File: {path.name}")
        for line in path.read_text(encoding="utf-8").splitlines():
            parts.append("+" + line)
    parts.append("*** End Patch")
    return "\n".join(parts) + "\n"


class Stub(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _send(self, delta, finish=None):
        payload = {"id": "s", "object": "chat.completion.chunk", "model": "stub",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
        if finish:
            payload["usage"] = {"prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}") if length else {}
        acts = sum(1 for m in body.get("messages", [])
                   if m.get("role") == "assistant" and m.get("tool_calls"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if acts == 0:
            self._send({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
                                        "function": {"name": "artifact",
                                                     "arguments": json.dumps({"patchText": patch_text()},
                                                                             ensure_ascii=False)}}]})
            self._send({}, "tool_calls")
        else:
            self._send({"content": "画好了。"})
            self._send({}, "stop")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": [{"id": "stub"}]}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    (HOME / "config" / "config.jsonc").write_text(json.dumps({
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub"}],
        "providers": [{"id": "stub", "display_name": "Stub",
                       "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
                       "protocol": "openai-chat", "api_key": "stub", "models": ["stub"],
                       "model_context_window": {"stub": 100000}}],
        "memory": {"enabled": False},
    }, ensure_ascii=False, indent=2), "utf-8")


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
    with urllib.request.urlopen(request, timeout=20) as response:
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


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    server = ThreadingHTTPServer(("127.0.0.1", STUB_PORT), Stub)
    Thread(target=server.serve_forever, daemon=True).start()
    report = {"files": [str(p) for p in FILES], "rendered": {}}
    errors = []
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    try:
        assert wait_http(f"{BASE}/api/config"), "daemon 没起来"
        time.sleep(1)
        api("POST", "/api/sessions", {"name": "渲染", "switch": True})
        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1400, "height": 1000})
            page.on("console", lambda m: errors.append(m.text)
                    if m.type == "error" and "404" not in m.text else None)
            page.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(
                k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            page.goto(BASE)
            page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
            page.wait_for_timeout(500)
            page.fill("#composerInput", "画")
            page.click("#sendButton")
            deadline = time.time() + 60
            while time.time() < deadline:
                if page.evaluate("Boolean(document.querySelector('#artifactToggleButton:not([hidden])'))"):
                    break
                page.wait_for_timeout(300)
            page.wait_for_timeout(2000)
            if page.evaluate("document.getElementById('artifactWorkspace').hidden"):
                page.click("#artifactToggleButton")
                page.wait_for_timeout(500)
            page.evaluate("() => document.getElementById('artifactMaximizeButton').click()")
            page.wait_for_timeout(400)
            for path in FILES:
                page.click("#artifactTitleButton")
                page.wait_for_timeout(250)
                rows = page.locator(".artifact-resource-menu button[role='menuitem']")
                hit = False
                for index in range(rows.count()):
                    if path.name in rows.nth(index).inner_text():
                        rows.nth(index).click()
                        hit = True
                        break
                if not hit:
                    page.keyboard.press("Escape")
                    report["rendered"][path.name] = "面板里没找到"
                    continue
                page.wait_for_timeout(3500)
                frame = page.frame_locator("#artifactView iframe")
                report["rendered"][path.name] = {
                    "canvas": frame.locator("canvas").count(),
                    "svg": frame.locator("svg").count(),
                }
                page.screenshot(path=str(OUT / f"{path.stem}.png"))
            browser.close()
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()
        server.shutdown()
    report["console_errors"] = errors[:20]
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), "utf-8")
    print(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"截图在 {OUT}")


if __name__ == "__main__":
    main()
