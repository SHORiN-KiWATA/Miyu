#!/usr/bin/env python3
"""OpenAI 兼容桩 + 隔离 daemon + PTY REPL,复现终端逐 delta 成段。
用法: MODE=seq|interleave|plain|empty BIN=<miyu> python3 repro_openai.py
"""
import fcntl
import json
import os
import pty
import shutil
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time
import urllib.request
from pathlib import Path

import pyte

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"])
MODE = os.environ.get("MODE", "seq")
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-wrap-repro-openai")).expanduser() / MODE
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18479"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18498"))
ROWS, COLS = 40, 120
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME), TERM="xterm-256color", LANG="zh_CN.UTF-8")


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{"id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
                       "protocol": "openai-chat", "api_key": "stub", "models": ["stub-model"]}],
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


class Repl:
    def __init__(self, log):
        self.log = open(log, "wb")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.proc = subprocess.Popen([str(BIN), "normal"], stdin=slave, stdout=slave, stderr=slave,
                                     env=ENV, cwd=str(HOME), close_fds=True, preexec_fn=os.setsid)
        os.close(slave)
        self.raw = bytearray()
        # 边收边喂给一个 pyte 屏幕:光标位置查询(ESC[6n)按真实光标应答。
        # 固定答 1;1 会让「挂起活动区 → println → 按光标位置重新挂回」这类
        # 流程在测具里错位,而真终端里是对的。
        self.live_screen = pyte.Screen(COLS, ROWS)
        self.live_stream = pyte.ByteStream(self.live_screen)
        self._stop = False
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        buf = b""
        while not self._stop:
            try:
                chunk = os.read(self.master, 4096)
            except OSError:
                break
            if not chunk:
                break
            self.raw += chunk
            self.log.write(chunk)
            self.log.flush()
            try:
                self.live_stream.feed(chunk)
            except Exception:
                pass
            buf = (buf + chunk)[-64:]
            if b"\x1b[6n" in buf:
                row = self.live_screen.cursor.y + 1
                col = self.live_screen.cursor.x + 1
                os.write(self.master, f"\x1b[{row};{col}R".encode())
                buf = buf.replace(b"\x1b[6n", b"")
            if b"\x1b[c" in buf or b"\x1b[0c" in buf:
                os.write(self.master, b"\x1b[?6c")
                buf = buf.replace(b"\x1b[c", b"").replace(b"\x1b[0c", b"")

    def send(self, line):
        os.write(self.master, line.encode() + b"\r")

    def close(self):
        self._stop = True
        try:
            os.write(self.master, b"\x04")
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def completed_turns(db):
    if not db.exists():
        return 0
    try:
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=5)
        n = conn.execute("SELECT COUNT(*) FROM turns WHERE status='completed'").fetchone()[0]
        conn.close()
        return n
    except sqlite3.OperationalError:
        return 0


def main():
    if HOME.exists():
        shutil.rmtree(HOME)
    RUNTIME.mkdir(parents=True, exist_ok=True)
    write_config()
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_reasoning.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT), MODE=MODE),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    repl = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"http://127.0.0.1:{PORT}/api/config", timeout=40), "daemon not up"
        time.sleep(1)
        repl = Repl(OUT / "raw.bin")
        db = HOME / "state" / "conversation.db"
        time.sleep(4)
        repl.send("输出当前使用的模型")
        deadline = time.time() + 60
        while time.time() < deadline and completed_turns(db) < 1:
            if repl.proc.poll() is not None:
                print("REPL exited early")
                break
            time.sleep(0.5)
        time.sleep(2)
    finally:
        if repl:
            repl.close()
        for p in (daemon, stub):
            if p:
                p.terminate()
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    p.kill()
    screen = pyte.Screen(COLS, ROWS)
    pyte.ByteStream(screen).feed(bytes(repl.raw))
    lines = [line.rstrip() for line in screen.display]
    (OUT / "screen.txt").write_text("\n".join(lines))
    shown = [l for l in lines if l.strip()]
    print(f"--- MODE={MODE} screen ---")
    print("\n".join(shown[-16:]))
    keys = ["问我的模型", "那还是不说", "有什么好讲", "要是", "你问的是", "我翻了你本机", "settings.json"]
    frag_lines = sum(1 for l in lines if any(k in l for k in keys))
    print(f"MODE={MODE} fragment lines: {frag_lines}  (1 = 正常, >=4 = 复现)")


if __name__ == "__main__":
    sys.exit(main())
