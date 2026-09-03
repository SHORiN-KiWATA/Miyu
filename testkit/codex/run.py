#!/usr/bin/env python3
"""codex 供应商中转的真机续传实测。

同一个直连 REPL 会话里连发两轮,断言第二轮请求带 resume <id>(前缀链命中,
只发增量)。会花两次真实 codex 调用,不进 CI;验收手跑:

    MIYU_HOME=/tmp/miyu-codex/home python3 testkit/codex/run.py

前提:MIYU_HOME 下 config.jsonc 的 active_provider 是 codex 协议供应商,本机 codex 已登录,daemon 未运行(直连互斥)。

"""

import fcntl
import glob
import json
import os
import pty
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MIYU_BIN = REPO / "target" / "debug" / "miyu"
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-codex/home"))


class Repl:
    def __init__(self, home: Path, log: Path):
        self.home = home
        self.db = home / "state" / "conversation.db"
        log.parent.mkdir(parents=True, exist_ok=True)
        self.log = open(log, "wb")
        env = dict(os.environ)
        env["MIYU_HOME"] = str(home)
        env["MIYU_DIRECT"] = "1"
        env["MIYU_LOG_REQUESTS"] = "1"
        env["TERM"] = "xterm-256color"
        env.setdefault("LANG", "zh_CN.UTF-8")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        self.proc = subprocess.Popen(
            [str(MIYU_BIN), "normal"],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
            preexec_fn=os.setsid,
        )
        os.close(slave)
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
            self.log.write(chunk)
            self.log.flush()
            buf = (buf + chunk)[-64:]
            if b"\x1b[6n" in buf:
                os.write(self.master, b"\x1b[1;1R")
                buf = buf.replace(b"\x1b[6n", b"")
            if b"\x1b[c" in buf or b"\x1b[0c" in buf:
                os.write(self.master, b"\x1b[?6c")
                buf = buf.replace(b"\x1b[c", b"").replace(b"\x1b[0c", b"")

    def send(self, line: str):
        os.write(self.master, line.encode("utf-8") + b"\r")

    def completed_turns(self):
        if not self.db.exists():
            return 0
        try:
            conn = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True, timeout=5)
            rows = conn.execute(
                "SELECT COUNT(*) FROM turns WHERE status='completed'"
            ).fetchall()
            conn.close()
            return rows[0][0]
        except sqlite3.OperationalError:
            return 0

    def wait_turns(self, n: int, timeout: float = 120):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.completed_turns() >= n:
                return
            if self.proc.poll() is not None:
                raise RuntimeError("REPL 进程提前退出")
            time.sleep(1)
        raise TimeoutError(f"等待第 {n} 轮完成超时")

    def close(self):
        self._stop = True
        try:
            os.write(self.master, b"\x04")  # Ctrl+D 脱离
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def codex_records():
    records = []
    for f in sorted(glob.glob(str(HOME / "cache" / "logs" / "requests-*.jsonl"))):
        for line in open(f):
            v = json.loads(line)
            if v.get("kind") == "codex" and v.get("scope") == "chat":
                records.append(v)
    return records


def main():
    base = len(codex_records())
    repl = Repl(HOME, HOME / "cache" / "logs" / "codex-resume-pty.log")
    try:
        time.sleep(3)
        start = repl.completed_turns()
        repl.send("回复字母 C")
        repl.wait_turns(start + 1)
        repl.send("回复字母 D")
        repl.wait_turns(start + 2)
    finally:
        repl.close()
    records = codex_records()[base:]
    assert len(records) >= 2, f"应有两条 codex 请求记录,实得 {len(records)}"
    first, second = records[0], records[-1]
    a1, a2 = first["body"]["args"], second["body"]["args"]
    print("turn1 resume:", "resume" in a1, "| turn2 resume:", "resume" in a2)
    print("turn2 stdin:", second["body"]["stdin"][:200])
    assert "resume" not in a1, "首轮不该续传"
    assert "resume" in a2, "第二轮应命中前缀链走 resume"
    assert "conversation-history" not in second["body"]["stdin"], "续传增量不该带历史转写"
    assert "字母 C" not in second["body"]["stdin"], "增量不该重放首轮输入"
    print("PASS: 同会话第二轮命中续传,只发增量")


if __name__ == "__main__":
    sys.exit(main())
