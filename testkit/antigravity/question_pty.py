#!/usr/bin/env python3
"""真机:中转线桥问答后,终端不应再出现黏住的「准备问题」(09-03 用户实录的回归探针)。

前提:隔离 home 已配 antigravity(或任一 CLI 中转)供应商,且 daemon 在跑——
直连模式下桥上没有 ask_question(它的注册门槛是 /dev/tty)。用法:

    MIYU_HOME=/tmp/miyu-agy/home XDG_RUNTIME_DIR=/tmp/miyu-agy/run \\
      target/debug/miyu daemon start --port 18300
    MIYU_HOME=/tmp/miyu-agy/home XDG_RUNTIME_DIR=/tmp/miyu-agy/run python3 testkit/antigravity/question_pty.py

证伪法:把 cli_relay::hidden_remote_tool 临时改成恒 false,面板之后会出现二十几处「准备问题」。

REPL(PTY)里让模型经桥问一个二选一问题,面板出现后按回车选第一项,
等模型接着用 run_command 跑完;然后把 PTY 原始字节回放进 pyte 屏幕,统计
「准备问题」出现的位置:应只在问题面板之前(或根本不出现),不该出现在
面板之后、后续工具之前。
"""
import fcntl, os, pty, re, sqlite3, struct, subprocess, sys, termios, threading, time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MIYU = REPO / "target/debug/miyu"
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-agy/home"))
LOG = HOME / "cache" / "logs" / "question-pty.log"


class Repl:
    def __init__(self):
        env = dict(os.environ)
        for k in ("XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
            env.pop(k, None)
        env.update(MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=os.environ.get("XDG_RUNTIME_DIR", "/tmp/miyu-agy/run"),
                   TERM="xterm-256color", LANG="zh_CN.UTF-8")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        self.proc = subprocess.Popen([str(MIYU), "normal"], stdin=slave, stdout=slave, stderr=slave,
                                     env=env, preexec_fn=os.setsid, close_fds=True)
        os.close(slave)
        self.buf = bytearray()
        self.log = open(LOG, "wb")
        self.lock = threading.Lock()
        threading.Thread(target=self.pump, daemon=True).start()

    def pump(self):
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            with self.lock:
                self.buf.extend(data)
            self.log.write(data); self.log.flush()
            if b"\x1b[6n" in data:
                os.write(self.master, b"\x1b[1;1R")
            if b"\x1b[c" in data:
                os.write(self.master, b"\x1b[?6c")

    def send(self, text):
        os.write(self.master, text.encode())

    def wait_for(self, needle, timeout):
        t0 = time.time()
        while time.time() - t0 < timeout:
            with self.lock:
                if needle.encode() in self.buf:
                    return True
            time.sleep(0.2)
        return False

    def completed_turns(self):
        db = HOME / "state/conversation.db"
        if not db.exists():
            return 0
        c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        try:
            return c.execute("select count(*) from turns where status='completed'").fetchone()[0]
        finally:
            c.close()

    def close(self):
        try:
            self.send("/exit\r")
            self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def main():
    repl = Repl()
    try:
        time.sleep(3)
        start = repl.completed_turns()
        repl.send("用 mcp_miyu_ask_question 工具问我喜欢红色还是蓝色(给两个选项),拿到我的回答后,再用 run_command 运行 echo AFTER-Q,最后一句话汇报颜色和命令输出\r")
        shown = repl.wait_for("红", 150)
        print("question panel shown:", shown)
        time.sleep(1.5)
        repl.send("\r")  # 选第一项
        t0 = time.time()
        while repl.completed_turns() < start + 1 and time.time() - t0 < 180:
            time.sleep(1)
        print("turn completed:", repl.completed_turns() >= start + 1)
        time.sleep(2)
    finally:
        repl.close()
    raw = LOG.read_bytes().decode("utf-8", "replace")
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[()][A-Z0-9]", "", raw)
    marks = [m.start() for m in re.finditer("准备问题", text)]
    q = text.find("红色")
    after = text.find("AFTER-Q")
    print("准备问题 offsets:", marks, "| panel at", q, "| AFTER-Q at", after)
    stray = [m for m in marks if q != -1 and m > q]
    print("stray 准备问题 after the panel:", len(stray))
    print("---- tail ----")
    print(text[-1200:])
    return 0 if not stray else 1


if __name__ == "__main__":
    sys.exit(main())
