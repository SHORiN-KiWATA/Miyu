#!/usr/bin/env python3
"""只测后台任务行点击：F4 → 点命令行 → 截图 → Esc → 点子代理行 → 截图。
    OUT=~/.cache/miyu-tui-probe MIYU_DEMO_LOG=~/.cache/miyu-tui-demo-events.log \
      testkit/kitty-image/run_headless.sh python3 testkit/tui-demo/kitty_probe_jobs.py <bin>
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

def rows_with(needle):
    text = subprocess.run(["kitten", "@", "--to", listen, "get-text"], capture_output=True, text=True).stdout
    return [i for i, l in enumerate(text.splitlines()) if needle in l]

proc = subprocess.Popen([BIN])
time.sleep(1.0)
send("\x1bOS")
time.sleep(3.0)
ys = rows_with("a1b2c3") + rows_with("b7c2d1")
print("job rows:", ys, file=sys.stderr, flush=True)
send(f"\x1b[<35;6;{ys[0]+1}M")  # 悬停到命令行
shot("job-hover")
mouse(0, 5, ys[0]); mouse(0, 5, ys[0], release=True)
time.sleep(2.0)
shot("job-cmd")
send("\x1b")
time.sleep(0.3)
mouse(0, 5, ys[1]); mouse(0, 5, ys[1], release=True)
time.sleep(3.0)
shot("job-agent")
send("\x1b")
time.sleep(0.2)
send("\x04")
try:
    proc.wait(timeout=3)
except subprocess.TimeoutExpired:
    proc.kill()
print("exit", proc.returncode)
