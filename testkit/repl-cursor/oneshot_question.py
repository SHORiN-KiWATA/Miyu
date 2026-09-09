#!/usr/bin/env python3
"""一次性客户端(shellhook 形态)在问题面板挂着时断线,下一轮还记不记得那一句。

沙箱 daemon + testkit/cli 的桩 LLM(用户消息带 ASK_QUESTION 就先问一个二选一;
桩把它看到的带 TK 标记的 user 消息条数回在正文里)。流程:
    1. PTY 里跑 `miyu "TK ASK_QUESTION …"`,等问题面板出现
    2. SIGKILL 客户端(模拟 shellhook 断开)
    3. 再跑 `miyu ask --output-format json "TK 第二句"`,读 user_count
修前:上一轮永远 running、被历史跳过,user_count=1;修后:那轮落成 interrupted 被回放,user_count=2。

用法:BIN=… OUT=~/.cache/miyu-oneshot-q python3 testkit/repl-cursor/oneshot_question.py
"""
import fcntl
import importlib.util
import json
import os
import pty
import signal
import sqlite3
import struct
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
OUT = Path(os.environ.get("OUT") or "~/.cache/miyu-oneshot-q").expanduser()
HOME = OUT / "home"
RUN = Path.home() / ".cache" / "miyu-oq-run"
PORT = int(os.environ.get("PORT", "18399"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18499"))

spec = importlib.util.spec_from_file_location("clitk", REPO / "testkit" / "cli" / "run.py")
clitk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clitk)
clitk.HOME, clitk.RUN, clitk.OUT, clitk.PORT, clitk.STUB_PORT = HOME, RUN, OUT / "cli-out", PORT, STUB_PORT
clitk.MIYU = BIN


def become_session_leader():
    # 问题面板要开 /dev/tty:PTY 必须是子进程的控制终端,光 setsid 不够。
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def run_oneshot_in_pty(message, kill_when, tag="first"):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    proc = subprocess.Popen([str(BIN), "--shell-intercept", "--shell", "fish", message], stdin=slave, stdout=slave, stderr=slave,
                            env=clitk.env({"TERM": "xterm-256color"}), preexec_fn=become_session_leader,
                            close_fds=True)
    os.close(slave)
    buf = bytearray()
    lock = threading.Lock()

    def pump():
        while True:
            try:
                data = os.read(master, 65536)
            except OSError:
                return
            if not data:
                return
            with lock:
                buf.extend(data)
            if b"\x1b[6n" in data:
                os.write(master, b"\x1b[1;1R")

    threading.Thread(target=pump, daemon=True).start()
    t0 = time.time()
    shown = False
    while time.time() - t0 < 90:
        with lock:
            text = buf.decode("utf-8", "replace")
        if kill_when in text:
            shown = True
            break
        if proc.poll() is not None:
            break
        time.sleep(0.2)
    if tag != "first":
        # 第二轮让它自然跑完。
        t1 = time.time()
        while proc.poll() is None and time.time() - t1 < 60:
            time.sleep(0.3)
    time.sleep(1.0)
    with lock:
        (OUT / f"pty-{tag}.txt").write_bytes(bytes(buf))
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
    proc.wait(timeout=10)
    return shown


def turn_statuses():
    dbs = list(HOME.rglob("conversation.db"))
    if not dbs:
        return []
    db = dbs[0]
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        return con.execute("select status from turns order by seq").fetchall()
    finally:
        con.close()


def main():
    assert BIN.exists(), BIN
    clitk.build_home()
    stub = subprocess.Popen([sys.executable, str(REPO / "testkit" / "cli" / "stub_llm.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = subprocess.Popen([str(BIN), "daemon", "--port", str(PORT)], env=clitk.env(),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    verdict = {}
    try:
        for _ in range(60):
            if clitk.find_socket():
                break
            time.sleep(0.5)
        assert clitk.find_socket(), "daemon socket never appeared"
        time.sleep(1.0)
        verdict["panel_shown"] = run_oneshot_in_pty("TK ASK_QUESTION 选个颜色", "红")
        time.sleep(3.0)
        verdict["statuses_after_kill"] = [s[0] for s in turn_statuses()]
        # 第二轮也用同一种一次性形态(`miyu "…"`),落在同一个会话里。
        run_oneshot_in_pty("TK 第二句", "user_count", tag="second")
        import re
        text = (OUT / "pty-second.txt").read_bytes().decode("utf-8", "replace")
        text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
        found = re.findall(r"user_count\D+(\d+)", text)
        summary = {"user_count": int(found[-1])} if found else {}
        verdict["second_turn_user_count"] = summary.get("user_count")
        verdict["statuses_final"] = [s[0] for s in turn_statuses()]
        verdict["ok"] = verdict["panel_shown"] and summary.get("user_count") == 2
    finally:
        subprocess.run([str(BIN), "daemon", "stop"], env=clitk.env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        stub.terminate()
    (OUT / "verdict.json").write_text(json.dumps(verdict, ensure_ascii=False, indent=2))
    print(json.dumps(verdict, ensure_ascii=False))
    return 0 if verdict.get("ok") else 1


if __name__ == "__main__":
    sys.exit(main())
