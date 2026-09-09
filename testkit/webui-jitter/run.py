#!/usr/bin/env python3
"""Safari 流式气泡尾部抖动的复现与 A/B:WebKitGTK(真 WebKit)对照 Chromium。

沙箱 daemon + 桩模型(不花额度),浏览器里真的从输入框发一条消息,让气泡
流式长上五六秒,逐帧记录 chatScroll 的 scrollTop/scrollHeight。量的是:
流式期间视口往回跳了几次、跳了多远、落后气泡底部最多多远。

    python3 testkit/webui-jitter/run.py            # 两个引擎都跑
    ENGINES=webkit python3 testkit/webui-jitter/run.py

前置:`cargo build`(静态资源编进二进制,改了 JS 必须重新构建);cage(无头
合成器)、PyGObject + webkitgtk-6.0;Chromium 对照要 python playwright。
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import metrics  # noqa: E402

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-webui-jitter/home"))
RUNTIME = os.environ.get("MIYU_JT_RUNTIME", "/tmp/mx-jt")
PORT = int(os.environ.get("MIYU_JT_PORT", "18421"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18496"))
OUT = Path(os.environ.get("OUT", Path.home() / ".cache" / "miyu-webui-jitter"))
TAG = os.environ.get("TAG", "run")
ENGINES = os.environ.get("ENGINES", "webkit,chromium").split(",")
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)


def api(path, body=None, method="GET"):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Origin": BASE},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        raw = response.read()
    return json.loads(raw or b"null")


def write_config():
    HOME.mkdir(parents=True, exist_ok=True)
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{
            "id": "stub",
            "display_name": "Stub",
            "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat",
            "api_key": "stub",
            "models": ["stub-model"],
        }],
        "memory": {"enabled": False},
        "tools": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def wait_http(url, timeout=20):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            time.sleep(0.2)
    return False


def run_webkit(out):
    """无头 cage 里跑 WebKitGTK 探针。cage 退出时探针已经写完样本。"""
    env = dict(os.environ)
    env.update({
        "WLR_BACKENDS": "headless",
        "WLR_LIBINPUT_NO_DEVICES": "1",
        "WLR_RENDERER": "pixman",
        "XDG_RUNTIME_DIR": os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"),
        "GDK_BACKEND": "wayland",
        "WEBKIT_DISABLE_COMPOSITING_MODE": "1",
        "WEBKIT_DISABLE_DMABUF_RENDERER": "1",
    })
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    probe = Path(__file__).parent / "probe_webkit.py"
    result = subprocess.run(
        ["cage", "--", sys.executable, str(probe), BASE, str(out)],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180,
    )
    (out / "webkit-stdout.txt").write_text(result.stdout, encoding="utf-8")
    return out / "webkit-samples.json"


def run_chromium(out):
    probe = Path(__file__).parent / "probe_chromium.py"
    result = subprocess.run(
        [sys.executable, str(probe), BASE, str(out)],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=180,
    )
    (out / "chromium-stdout.txt").write_text(result.stdout, encoding="utf-8")
    return out / "chromium-samples.json"


def main():
    if not BIN.exists():
        print(f"! 先 cargo build:{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists():
        shutil.rmtree(HOME)
    Path(RUNTIME).mkdir(exist_ok=True)
    write_config()
    out = OUT / TAG
    out.mkdir(parents=True, exist_ok=True)

    stub = subprocess.Popen(
        [sys.executable, str(Path(__file__).parent / "stub_llm.py")],
        env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    daemon = None
    report = {}
    try:
        if not wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"):
            print("! 桩模型没起来", file=sys.stderr)
            return 2
        daemon = subprocess.Popen(
            [str(BIN), "__daemon", "--port", str(PORT)],
            env=ENV, cwd=str(HOME),
            stdout=(out / "daemon.log").open("w"), stderr=subprocess.STDOUT,
        )
        if not wait_http(f"{BASE}/api/config", timeout=30):
            print("! daemon 没起来", file=sys.stderr)
            return 2
        for engine in ENGINES:
            # 每个引擎一个新会话,免得上一轮的长回复留在时间线里。
            session = api("/api/sessions", {"name": f"抖动走查 {engine}", "switch": True}, "POST")
            session_id = session.get("session_id") or session.get("session", {}).get("session_id")
            print(f"· {engine}: 会话 {session_id}", flush=True)
            path = run_webkit(out) if engine == "webkit" else run_chromium(out)
            payload = json.loads(path.read_text()) if path.exists() else {}
            samples = payload.get("samples", payload if isinstance(payload, list) else [])
            report[engine] = metrics.summarize(samples)
            print(f"  {json.dumps(report[engine], ensure_ascii=False)}", flush=True)
    finally:
        for process in (daemon, stub):
            if process is None:
                continue
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
    (out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
