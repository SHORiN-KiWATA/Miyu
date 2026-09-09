#!/usr/bin/env python3
"""REPL 真机走查:上键历史带回占位符 + footer 的每秒 token。

沙箱 daemon + 桩模型(不花额度),真终端里粘 4 行文本 → 回车发出 → 按上键
回忆。看两件事:

1. 粘贴时输入框显示 `[粘贴 1: ~4 行]`,发出后按上键回忆到的仍是这个占位符
   (不是展开后的四行裸文本);
2. footer 出现 `NNN tok/s`(桩模型分块吐字,回合层量得出来)。

    python3 testkit/repl-smoke/run.py

产物在 ~/.cache/miyu-repl-smoke/:raw.bin、report.json、daemon.log。
"""

import json
import os
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import termios
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-repl-smoke/home"))
RUNTIME = os.environ.get("MIYU_RS_RUNTIME", "/tmp/mx-rs")
PORT = int(os.environ.get("MIYU_RS_PORT", "18423"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18498"))
OUT = Path(os.environ.get("OUT", Path.home() / ".cache" / "miyu-repl-smoke"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)

PASTED_LINES = ["第一行走查", "第二行走查", "第三行走查", "第四行走查"]
PLACEHOLDER = "粘贴 1"


def write_config():
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


def spawn_repl():
    master, slave = pty.openpty()
    import fcntl
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))

    def child_setup():
        os.setsid()
        fcntl.ioctl(1, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        # 裸 `miyu` 在真终端里先弹模式选择再退出;走查要的是普通模式的 REPL。
        [str(BIN), "normal"], stdin=slave, stdout=slave, stderr=slave,
        env=ENV, cwd=str(HOME), preexec_fn=child_setup, close_fds=True,
    )
    os.close(slave)
    return process, master


def drain(master, seconds, sink):
    """读 seconds 秒,把内容追加进 sink 并返回这段新增的原始字节。"""
    collected = bytearray()
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.1)
        if not ready:
            continue
        try:
            data = os.read(master, 65536)
        except OSError:
            break
        if not data:
            break
        collected += data
        sink += data
    return bytes(collected)


def drain_until(master, sink, marker, timeout):
    """读到 marker 出现为止(或超时),返回 (原始字节, 耗时秒)。"""
    collected = bytearray()
    started = time.time()
    while time.time() - started < timeout:
        collected += drain(master, 0.5, sink)
        if marker in strip_ansi(bytes(collected)):
            break
    return bytes(collected), round(time.time() - started, 2)


def strip_ansi(raw):
    text = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", raw)
    text = re.sub(rb"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", b"", text)
    return text.decode("utf-8", "replace")


def main():
    if not BIN.exists():
        print(f"! 先 cargo build:{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists():
        shutil.rmtree(HOME)
    Path(RUNTIME).mkdir(exist_ok=True)
    OUT.mkdir(parents=True, exist_ok=True)
    write_config()

    stub = subprocess.Popen(
        [sys.executable, str(Path(__file__).parent / "stub_llm.py")],
        env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    daemon = None
    repl = None
    try:
        if not wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"):
            print("! 桩模型没起来", file=sys.stderr)
            return 2
        daemon = subprocess.Popen(
            [str(BIN), "__daemon", "--port", str(PORT)],
            env=ENV, cwd=str(HOME),
            stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT,
        )
        if not wait_http(f"{BASE}/api/config", timeout=30):
            print("! daemon 没起来", file=sys.stderr)
            return 2

        repl, master = spawn_repl()
        sink = bytearray()
        report = {}
        drain(master, 3.0, sink)

        paste = ("\x1b[200~" + "\r".join(PASTED_LINES) + "\x1b[201~").encode()
        os.write(master, paste)
        pasted, _ = drain_until(master, sink, PLACEHOLDER, 3.0)
        report["placeholder_on_paste"] = PLACEHOLDER in strip_ansi(pasted)

        os.write(master, b"\r")
        answered, reply_seconds = drain_until(master, sink, "走查的回复", 30.0)
        answered_text = strip_ansi(answered)
        report["reply_seen"] = "走查的回复" in answered_text
        report["reply_seconds"] = reply_seconds
        speed = re.search(r"([0-9.]+) tok/s", answered_text)
        report["footer_speed"] = speed.group(0) if speed else None

        # 等渲染安顿下来再按上键:回复刚打完时编辑器还在重绘,按键会被吞掉。
        drain(master, 1.5, sink)
        os.write(master, b"\x1b[A")
        recalled = drain(master, 2.0, sink)
        recalled_text = strip_ansi(recalled)
        report["placeholder_on_recall"] = PLACEHOLDER in recalled_text
        report["raw_text_on_recall"] = any(line in recalled_text for line in PASTED_LINES)

        os.write(master, b"\x03")
        drain(master, 0.5, sink)
        os.write(master, b"\x03")
        drain(master, 0.5, sink)
        report["repl_alive"] = repl.poll() is None

        (OUT / "raw.bin").write_bytes(bytes(sink))
        (OUT / "report.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        print(json.dumps(report, ensure_ascii=False))
        return 0
    finally:
        for process in (repl, daemon, stub):
            if process is None:
                continue
            try:
                process.send_signal(signal.SIGTERM)
                process.wait(timeout=5)
            except Exception:
                try:
                    process.kill()
                except Exception:
                    pass


if __name__ == "__main__":
    sys.exit(main())
