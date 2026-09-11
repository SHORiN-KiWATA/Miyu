#!/usr/bin/env python3
"""用 pyte 模拟终端驱动 demo：命令列表 / 排队 / 鼠标拖选(SGR 序列) / 弹层，逐步抓屏，
截获 OSC 52 剪贴板内容，末尾量 RSS。
用法: python3 drive.py <binary> [cols] [rows] [--styles]
"""
import base64, os, pty, re, sys, time, struct, fcntl, termios, select
import pyte

args = [a for a in sys.argv[1:] if not a.startswith("--")]
SHOW_STYLES = "--styles" in sys.argv
BIN = args[0]
COLS = int(args[1]) if len(args) > 1 else 110
ROWS = int(args[2]) if len(args) > 2 else 32

screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)
clipboard = []

pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.environ.pop("KITTY_WINDOW_ID", None)
    os.execvp(BIN, [BIN])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

OSC52 = re.compile(rb"\x1b\]52;c;([A-Za-z0-9+/=]*)\x07")

def pump(seconds):
    t = time.time()
    while time.time() - t < seconds:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            for m in OSC52.finditer(data):
                clipboard.append(base64.b64decode(m.group(1)).decode("utf-8", "replace"))
            stream.feed(data)

def snap(title):
    print(f"\n===== {title} =====")
    for row in screen.display:
        print(row.rstrip())
    if SHOW_STYLES:
        for y in range(ROWS):
            line = screen.buffer[y]
            runs, cur, text = [], None, ""
            for x in range(COLS):
                c = line[x]
                key = (c.fg, c.bold, c.reverse)
                if key != cur:
                    if cur is not None and text.strip():
                        runs.append((cur, text))
                    cur, text = key, c.data
                else:
                    text += c.data
            if cur is not None and text.strip():
                runs.append((cur, text))
            if any(k != ("default", False, False) for k, _ in runs):
                print(f"  {y:02d}: " + " | ".join(f"{t.strip()[:24]!r}<{k[0]}{' B' if k[1] else ''}{' R' if k[2] else ''}>" for k, t in runs))

def key(b, wait=0.15):
    os.write(fd, b)
    pump(wait)

def mouse(btn, x, y, release=False):
    # SGR 鼠标：列/行从 1 开始
    key(f"\x1b[<{btn};{x+1};{y+1}{'m' if release else 'M'}".encode(), 0.1)

def rss():
    with open(f"/proc/{pid}/smaps_rollup") as f:
        d = {}
        for line in f:
            k, _, v = line.partition(":")
            v = v.split()
            if v and v[-1] == "kB":
                d[k] = int(v[0]) / 1024
    return d

UP, DOWN, LEFT, RIGHT = b"\x1b[A", b"\x1b[B", b"\x1b[D", b"\x1b[C"
pump(0.5)
snap("启动：重放历史，底部留一行空")
key(b"/")
snap("输入 / ：命令列表锚在输入框上方")
key(DOWN); key(DOWN)
snap("↓↓ 选到第三项")
key(b"\t")
snap("Tab 补全到输入框")
key(b"\x1b")
key(b"/s")
snap("/s 过滤")
key(b"\r")
snap("Enter 执行 /session → 会话选择器")
key(b"\x1b")
# 鼠标拖选：从第 1 行(用户消息「发一个链接」所在 ┃ 行)拖到第 4 行
mouse(0, 0, 2)          # 按下在 ┃ 装饰列上
mouse(32, 20, 4)        # 拖
snap("拖选中（反显不含左侧竖条）")
mouse(0, 20, 4, release=True)
pump(0.3)
snap("松开：输入框上方弹出通知小悬浮窗")
print("剪贴板内容:", repr(clipboard[-1]) if clipboard else "（无）")
pump(2.8)
snap("2.5s 后通知自动消失")
key("选中我这段输入试试".encode())
# 输入框行：tail 高 5（bar/输入/bar/footer/空行），输入行 y = ROWS-5+1
iy = ROWS - 5 + 1
mouse(0, 1, iy)
mouse(32, 9, iy)
snap("在输入框里拖选（反显不含竖条）")
mouse(0, 9, iy, release=True)
pump(0.3)
print("输入框剪贴板内容:", repr(clipboard[-1]) if clipboard else "（无）")
key(b"\x03")
key("这是一条测试输入，看看一整轮".encode())
key(b"\r", 2.0)
key("回复中再发一条，看排队".encode())
key(b"\r", 1.0)
snap("排队气泡与输入区之间有空行")
pump(17.0)
snap("两轮都完成")
key(b"/img\r")
snap("/img（非 kitty 终端退回文字标签）")
key(b"\x1bOP")
snap("/help")
key(b"\x1b")
m = rss()
print(f"\n内存（{COLS}x{ROWS}）: Rss={m['Rss']:.1f}MB Anon={m['Anonymous']:.1f}MB Pss={m['Pss']:.1f}MB")
key(b"\x04", 0.5)
_, status = os.waitpid(pid, 0)
print("exit status:", os.waitstatus_to_exitcode(status))
