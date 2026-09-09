#!/usr/bin/env python3
"""shellhook 形态的 ask_question 走查:面板会不会自己消失/回合会不会被取消。

复现的是「fish hook 把命令行管道喂给 miyu」那一刻:stdin 是管道、stdout 是
终端、进程有控制终端(/dev/tty)。桩模型立刻提一个问题,脚本什么都不按,
几秒后再敲两下方向键,记录面板活了多久、回合有没有被取消。

    python3 testkit/shellhook-question/run.py                # 只看着,不按键
    KEYS=arrows python3 testkit/shellhook-question/run.py    # 3 秒后敲 ↑ ↓
    KEYS=answer python3 testkit/shellhook-question/run.py    # ↑ 之后回车提交

产物在 ~/.cache/miyu-shellhook-question/:raw.bin(终端原始输出)、report.json。
"""

import json
import os
import pty
import select
import shutil
import signal
import subprocess
import sys
import termios
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-shellhook-question/home"))
RUNTIME = os.environ.get("MIYU_SHQ_RUNTIME", "/tmp/mx-shq")
PORT = int(os.environ.get("MIYU_SHQ_PORT", "18422"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18497"))
OUT = Path(os.environ.get("OUT", Path.home() / ".cache" / "miyu-shellhook-question"))
OBSERVE_SECONDS = float(os.environ.get("OBSERVE_SECONDS", "12"))
KEYS = os.environ.get("KEYS", "")
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)

PANEL_MARKERS = ("这是一条走查用的问题", "确认")
CANCEL_MARKERS = ("已取消", "cancelled")


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


def spawn_shellhook(message):
    """stdin 是管道、stdout 是终端、进程持有控制终端——和 fish hook 一模一样。"""
    master, slave = pty.openpty()
    # 不给窗口尺寸的话 ioctl 返回 0x0,面板按 1 列宽画,只能看到一个字符。
    import fcntl
    import struct
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    read_end, write_end = os.pipe()

    def child_setup():
        os.setsid()
        import fcntl
        fcntl.ioctl(1, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [str(BIN), "--shell-intercept", "--shell", "fish", "--stdin"],
        stdin=read_end, stdout=slave, stderr=slave, env=ENV, cwd=str(HOME),
        preexec_fn=child_setup, close_fds=True,
    )
    os.close(slave)
    os.close(read_end)
    os.write(write_end, message.encode())
    os.close(write_end)
    return process, master


def observe(process, master):
    chunks = []
    started = time.time()
    keys_sent = False
    exited_at = None
    while time.time() - started < OBSERVE_SECONDS:
        if not keys_sent and KEYS and time.time() - started >= 3:
            # ↑ 然后 ↓:合法操作,不该有任何后果。answer 再敲一次回车提交。
            os.write(master, b"\x1b[A")
            time.sleep(0.4)
            os.write(master, b"\x1b[B")
            if KEYS == "answer":
                time.sleep(0.4)
                os.write(master, b"\r")
            keys_sent = True
        ready, _, _ = select.select([master], [], [], 0.2)
        if ready:
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            chunks.append((round(time.time() - started, 2), data))
        if process.poll() is not None and exited_at is None:
            exited_at = round(time.time() - started, 2)
    raw = b"".join(data for _, data in chunks)
    text = raw.decode("utf-8", "replace")
    return {
        "raw": raw,
        "chunks": chunks,
        "panel_seen": any(marker in text for marker in PANEL_MARKERS),
        "cancel_seen": any(marker in text for marker in CANCEL_MARKERS),
        "exited_at": exited_at,
        "returncode": process.poll(),
    }


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
    client = None
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
        client, master = spawn_shellhook("帮我确认一件事\n")
        result = observe(client, master)
        (OUT / "raw.bin").write_bytes(result.pop("raw"))
        (OUT / "chunks.json").write_text(
            json.dumps([[t, data.decode("utf-8", "replace")] for t, data in result.pop("chunks")],
                       ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        (OUT / "report.json").write_text(
            json.dumps(result, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        print(json.dumps(result, ensure_ascii=False))
        return 0
    finally:
        for process in (client, daemon, stub):
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
