#!/usr/bin/env python3
"""在真 kitty（无头 cage）里跑 demo，用 kitty 远程控制打字（含 SGR 鼠标序列），grim 逐状态截图。

    OUT=~/.cache/miyu-tui-probe testkit/kitty-image/run_headless.sh \
        python3 testkit/tui-demo/kitty_probe.py ~/.cache/miyu-tui-demo-target/release/miyu-tui-demo

产物：$OUT/tui-{start,slash,select,copied,img,turn,session,help}.png
"""
import os, subprocess, sys, time

BIN = sys.argv[1]
OUT = os.environ.get("OUT") or os.path.expanduser("~/.cache/miyu-tui-probe")
os.makedirs(OUT, exist_ok=True)
listen = os.environ["KITTY_LISTEN_ON"]

def send(text):
    subprocess.run(["kitten", "@", "--to", listen, "send-text", text], check=True)

def mouse(btn, x, y, release=False):
    send(f"\x1b[<{btn};{x+1};{y+1}{'m' if release else 'M'}")

def shot(name):
    time.sleep(0.4)
    subprocess.run(["grim", f"{OUT}/tui-{name}.png"], check=True)

proc = subprocess.Popen([BIN])
time.sleep(1.0)
shot("start")
send("/")
send("\x1b[B\x1b[B")
shot("slash")
send("\x1b")
time.sleep(0.2)
mouse(0, 0, 1)
mouse(32, 30, 3)
shot("select")
mouse(0, 30, 3, release=True)
shot("copied")
send("/img\r")
time.sleep(1.0)
shot("img")
send("看看这一轮的画面\r")
time.sleep(10.5)
shot("turn")
send("\x1bOS")
time.sleep(4.0)
mouse(0, 5, 34); mouse(0, 5, 34, release=True)
shot("job-cmd")
send("\x1b")
time.sleep(0.2)
mouse(0, 5, 35); mouse(0, 5, 35, release=True)
time.sleep(3.0)
shot("job-agent")
send("\x1b")
time.sleep(0.2)
send("\x1bOS")
time.sleep(0.2)
send("/session\r")
time.sleep(0.4)
send("\x1b[B\x1b[B")
shot("session")
send("\x1b")
time.sleep(0.2)
send("\x1bOP")
shot("help")
send("\x1b")
time.sleep(0.2)
send("\x04")
try:
    proc.wait(timeout=3)
except subprocess.TimeoutExpired:
    proc.kill()
print("exit", proc.returncode)
