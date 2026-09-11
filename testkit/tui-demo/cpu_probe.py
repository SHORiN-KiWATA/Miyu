#!/usr/bin/env python3
"""量 demo 的 CPU：空闲 10s 与回合中 10s 的 jiffies 差分，以及输出字节数。
用法: python3 cpu_probe.py <binary> [cols] [rows]
"""
import os, pty, sys, time, struct, fcntl, termios, select

BIN = sys.argv[1]
COLS = int(sys.argv[2]) if len(sys.argv) > 2 else 110
ROWS = int(sys.argv[3]) if len(sys.argv) > 3 else 30
HZ = os.sysconf("SC_CLK_TCK")

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.execvp(BIN, [BIN])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

def jiffies():
    with open(f"/proc/{pid}/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return int(parts[11]) + int(parts[12])

def pump(seconds):
    n = 0
    t = time.time()
    while time.time() - t < seconds:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                n += len(os.read(fd, 65536))
            except OSError:
                break
    return n

pump(0.5)
# 先跑两轮把历史撑起来
for i in range(2):
    os.write(fd, f"第 {i+1} 轮".encode() + b"\r")
    pump(9.5)
j0 = jiffies(); b0 = pump(10.0); j1 = jiffies()
print(f"空闲 10s: CPU {(j1-j0)/HZ*10:.1f}%  输出 {b0} B")
os.write(fd, "第 3 轮".encode() + b"\r")
j0 = jiffies(); b1 = pump(10.0); j1 = jiffies()
print(f"回合中 10s: CPU {(j1-j0)/HZ*10:.1f}%  输出 {b1} B")
os.write(fd, b"\x04")
pump(0.5)
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass
