#!/usr/bin/env python3
"""REPL 回车提交时的光标轨迹探针。

在 PTY 里跑 miyu REPL(沙箱 MIYU_HOME + 独立端口 daemon + 桩 LLM),按 kitty 的语义
回放 miyu 发出的每个 read 块:
  - ?2026h 开始暂停渲染:光标位置与可见性都取快照(kitty screen_pause_rendering)
  - ?2026l 或 2s 超时结束暂停
  - 每个 read 块结束 = 一个可能的渲染点(kitty 按 input_delay 攒批解析,块粒度是悲观上界)
记录每个渲染点上「用户会看到的光标」(位置/可见),找出回车之后出现在屏幕左下角
(或任何非输入框位置)且可见的帧。

用法:python3 cursor_probe.py [--direct] [--rows 40] [--cols 120] [--turns 3]
产物:$OUT/pty.bin(原始字节)、$OUT/chunks.jsonl(块时间线)、$OUT/report.txt
"""
import argparse
import fcntl
import importlib.util
import json
import os
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import termios
import threading
import time
from pathlib import Path

import pyte

REPO = Path("/home/shorin/Documents/github/Miyu")
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
BASE = Path(os.environ.get("OUT") or Path.home() / ".cache" / "miyu-cursor-probe")
HOME = BASE / "home"
RUN = Path.home() / ".cache" / "miyu-cp-run"  # SUN_LEN 限制,路径要短
OUT = BASE / "out"
PORT = int(os.environ.get("PORT", "18397"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18497"))
HERE = Path(__file__).resolve().parent

spec = importlib.util.spec_from_file_location("clitk", REPO / "testkit" / "cli" / "run.py")
clitk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clitk)
clitk.HOME, clitk.RUN, clitk.OUT, clitk.PORT, clitk.STUB_PORT = HOME, RUN, OUT, PORT, STUB_PORT
clitk.MIYU = MIYU

CSI_RE = re.compile(rb"\x1b\[(\?)?([0-9;]*)([A-Za-z@`])")


def env(extra=None):
    e = clitk.env(extra)
    e["TERM"] = "xterm-kitty"
    e["COLORTERM"] = "truecolor"
    return e


class Term:
    """pyte 屏幕 + kitty 暂停语义。"""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.paused_at = None
        self.snap = None  # (x, y, visible)
        self.pending = b""
        self.errors = []

    def feed(self, data, now):
        # 先按序列扫 2026/25 模式(pyte 不认 2026),再整块喂 pyte。
        buf = self.pending + data
        pos = 0
        for m in CSI_RE.finditer(buf):
            if m.group(1) == b"?" and m.group(3) in (b"h", b"l"):
                params = m.group(2).split(b";")
                # 先把这段之前的字节喂进去,让快照取到正确的光标。
                self.stream.feed(buf[pos:m.start()])
                pos = m.end()
                for p in params:
                    if p == b"2026":
                        if m.group(3) == b"h":
                            if self.paused_at is not None and now - self.paused_at < 2.0:
                                self.errors.append((now, "2026h while paused (nested)"))
                            else:
                                self.paused_at = now
                                self.snap = self.cursor_live()
                        else:
                            if self.paused_at is None:
                                self.errors.append((now, "2026l while not paused"))
                            self.paused_at = None
                            self.snap = None
                self.stream.feed(buf[m.start():m.end()])
        # 末尾可能有被切断的序列,留到下一块。
        tail = buf[pos:]
        cut = tail.rfind(b"\x1b")
        if cut != -1 and CSI_RE.search(tail[cut:]) is None and len(tail) - cut < 32:
            self.stream.feed(tail[:cut])
            self.pending = tail[cut:]
        else:
            self.stream.feed(tail)
            self.pending = b""

    def cursor_live(self):
        c = self.screen.cursor
        return (c.x, c.y, not c.hidden)

    def cursor_shown(self, now):
        if self.paused_at is not None:
            if now - self.paused_at >= 2.0:
                self.paused_at = None
                self.snap = None
            else:
                return self.snap + (True,)
        return self.cursor_live() + (False,)


class Repl:
    def __init__(self, rows, cols, direct):
        self.rows, self.cols = rows, cols
        e = env({"MIYU_DIRECT": "1"} if direct else None)
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.proc = subprocess.Popen([str(MIYU)], stdin=slave, stdout=slave, stderr=slave,
                                     env=e, preexec_fn=os.setsid, close_fds=True, cwd=str(BASE))
        os.close(slave)
        self.chunks = []  # (t, bytes)
        self.term = Term(rows, cols)
        self.lock = threading.Lock()
        self.t0 = time.monotonic()
        self.alive = True
        threading.Thread(target=self.pump, daemon=True).start()

    def pump(self):
        while True:
            try:
                data = os.read(self.master, 1 << 16)
            except OSError:
                self.alive = False
                return
            if not data:
                self.alive = False
                return
            now = time.monotonic() - self.t0
            with self.lock:
                self.chunks.append((now, data))
                self.term.feed(data, now)
                # 应答:光标位置查询按真实模拟位置答;DA 答 VT220。
                reply = b""
                for _ in range(data.count(b"\x1b[6n")):
                    c = self.term.screen.cursor
                    reply += f"\x1b[{c.y + 1};{c.x + 1}R".encode()
                if b"\x1b[c" in data or b"\x1b[0c" in data:
                    reply += b"\x1b[?62;22c"
                if b"\x1b[?u" in data:
                    reply += b"\x1b[?0u"
            if reply:
                os.write(self.master, reply)

    def send(self, text):
        os.write(self.master, text.encode())

    def display(self):
        with self.lock:
            return "\n".join(self.term.screen.display)

    def wait_display(self, pred, timeout):
        t0 = time.time()
        while time.time() - t0 < timeout:
            if pred(self.display()):
                return True
            time.sleep(0.05)
        return False

    def quiet(self, gap=0.6, timeout=30):
        """等输出安静 gap 秒。"""
        t0 = time.time()
        while time.time() - t0 < timeout:
            with self.lock:
                last = self.chunks[-1][0] if self.chunks else 0
            if time.monotonic() - self.t0 - last > gap:
                return True
            time.sleep(0.05)
        return False

    def mark(self):
        return time.monotonic() - self.t0

    def close(self):
        try:
            self.send("/exit\r")
            self.proc.wait(timeout=8)
        except Exception:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except Exception:
                pass


def analyze(chunks, rows, cols, marks, report):
    """离线回放:每块结束一个渲染点。"""
    term = Term(rows, cols)
    frames = []
    for t, data in chunks:
        term.feed(data, t)
        x, y, vis, paused = term.cursor_shown(t)
        frames.append((t, x, y, vis, paused, len(data)))
    lines = []
    lines.append(f"chunks={len(chunks)} frames={len(frames)} rows={rows} cols={cols}")
    if term.errors:
        lines.append("sync errors:")
        for t, msg in term.errors:
            lines.append(f"  t={t:8.3f} {msg}")
    for i, (label, t_enter, t_end) in enumerate(marks):
        lines.append(f"\n== {label}: enter at t={t_enter:.3f}, window to {t_end:.3f} ==")
        window = [f for f in frames if t_enter - 0.05 <= f[0] <= t_end]
        seen = None
        suspicious = []
        for t, x, y, vis, paused, n in window:
            state = (x, y, vis, paused)
            if state != seen:
                lines.append(f"  t={t:8.3f} cursor=({x},{y}) {'VISIBLE' if vis else 'hidden '}"
                             f" {'[paused-snapshot]' if paused else ''} bytes={n}")
                seen = state
            # kitty 的 cursor_trail 不看光标隐不隐藏:只要"终端认为的光标位置"
            # (暂停中取快照)落到最后一行,拖尾就会画过去。可见与否都算。
            if y == rows - 1:
                suspicious.append((round(t, 3), x, y, "visible" if vis else "hidden"))
        if suspicious:
            span = suspicious[-1][0] - suspicious[0][0]
            lines.append(f"  !! cursor parked on the last row: {len(suspicious)} frame(s), span {span:.3f}s: {suspicious[:5]}")
        else:
            lines.append("  OK: cursor never parked on the last row in this window")
    report.write_text("\n".join(lines), encoding="utf-8")
    print("\n".join(lines))


def dump_chunks(chunks, path):
    with path.open("w", encoding="utf-8") as f:
        for t, data in chunks:
            f.write(json.dumps({"t": round(t, 4), "n": len(data),
                                "head": data[:200].decode("utf-8", "replace")}, ensure_ascii=False) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--direct", action="store_true")
    ap.add_argument("--rows", type=int, default=40)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--turns", type=int, default=3)
    ap.add_argument("--lines", type=int, default=30, help="每轮桩回复的行数")
    ap.add_argument("--keep-home", action="store_true")
    args = ap.parse_args()
    assert MIYU.exists(), MIYU
    if not args.keep_home:
        clitk.build_home()
        # 桩模型指向本探针自己的桩。
        cfg_path = HOME / "config" / "config.jsonc"
        cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
        cfg["providers"][0]["base_url"] = f"http://127.0.0.1:{STUB_PORT}/v1"
        cfg_path.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    OUT.mkdir(parents=True, exist_ok=True)
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_long.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        if not args.direct:
            daemon = subprocess.Popen([str(MIYU), "daemon", "--port", str(PORT)], env=env(),
                                      stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
            for _ in range(60):
                if clitk.find_socket():
                    break
                time.sleep(0.5)
            assert clitk.find_socket(), "daemon socket never appeared"
            time.sleep(1.0)
        repl = Repl(args.rows, args.cols, args.direct)
        marks = []
        try:
            ok = repl.wait_display(lambda d: "┃" in d, 30)
            print("input box shown:", ok)
            repl.quiet(0.8, 20)
            for i in range(args.turns):
                repl.send(f"TK turn {i + 1} LINES={args.lines}")
                time.sleep(0.4)
                repl.quiet(0.4, 10)
                t_enter = repl.mark()
                repl.send("\r")
                # 等回复结束:桩最后一行出现且输出安静。
                repl.wait_display(lambda d: f"line {args.lines} of {args.lines}" in d, 60)
                repl.quiet(0.8, 30)
                marks.append((f"turn {i + 1}", t_enter, repl.mark()))
                (OUT / f"screen-after-{i + 1}.txt").write_text(repl.display(), encoding="utf-8")
        finally:
            repl.close()
        with repl.lock:
            chunks = list(repl.chunks)
        (OUT / "pty.bin").write_bytes(b"".join(d for _, d in chunks))
        dump_chunks(chunks, OUT / "chunks.jsonl")
        analyze(chunks, args.rows, args.cols, marks, OUT / "report.txt")
    finally:
        if daemon:
            subprocess.run([str(MIYU), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        stub.terminate()


if __name__ == "__main__":
    main()
