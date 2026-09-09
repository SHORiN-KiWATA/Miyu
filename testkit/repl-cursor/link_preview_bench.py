#!/usr/bin/env python3
"""量 WebUI 链接卡片后端(/api/link-preview)的冷/热延迟,做 A/B。

沙箱 daemon(不需要 LLM),对同一批链接各请求两次:第一次冷(真抓),第二次热(缓存)。
用法:BIN=… OUT=~/.cache/miyu-linkbench-x python3 testkit/repl-cursor/link_preview_bench.py
"""
import importlib.util
import json
import os
import subprocess
import sys
import time
import urllib.parse
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
OUT = Path(os.environ.get("OUT") or "~/.cache/miyu-linkbench").expanduser()
HOME = OUT / "home"
RUN = Path.home() / ".cache" / "miyu-lb-run"
PORT = int(os.environ.get("PORT", "18396"))
URLS = os.environ.get("URLS", ",".join([
    "https://b23.tv/BV1GJ411x7h7",
    "https://www.bilibili.com/video/BV1GJ411x7h7",
    "https://github.com/kovidgoyal/kitty",
])).split(",")

spec = importlib.util.spec_from_file_location("clitk", REPO / "testkit" / "cli" / "run.py")
clitk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clitk)
clitk.HOME, clitk.RUN, clitk.OUT, clitk.PORT, clitk.STUB_PORT = HOME, RUN, OUT / "cli-out", PORT, 18496
clitk.MIYU = BIN


def preview(url):
    base = f"http://127.0.0.1:{PORT}"
    request = urllib.request.Request(
        f"{base}/api/link-preview?url={urllib.parse.quote(url, safe='')}",
        headers={"Origin": base},
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            body = json.loads(response.read() or b"null")
        status = "ok" if body and body.get("title") else f"empty:{str(body)[:60]}"
    except Exception as error:
        status = f"error:{error}"
    return round(time.monotonic() - t0, 3), status


def main():
    clitk.build_home()
    daemon = subprocess.Popen([str(BIN), "daemon", "--port", str(PORT)], env=clitk.env(),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    rows = []
    try:
        for _ in range(60):
            if clitk.find_socket():
                break
            time.sleep(0.5)
        time.sleep(1.5)
        for url in URLS:
            cold, status = preview(url)
            warm, _ = preview(url)
            rows.append({"url": url, "cold_s": cold, "warm_s": warm, "status": status})
            print(json.dumps(rows[-1], ensure_ascii=False), flush=True)
    finally:
        subprocess.run([str(BIN), "daemon", "stop"], env=clitk.env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
    (OUT / "bench.json").write_text(json.dumps(rows, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
