#!/usr/bin/env python3
"""在无头 kitty 里画几种「无竖线装饰」候选样式,截图 + 用 kitten get-text 看复制会得到什么。

跑法:OUT=~/.cache/miyu-copy-styles testkit/kitty-image/run_headless.sh python3 copy_styles.py
"""
import os
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, "/home/shorin/Documents/github/Miyu/testkit/kitty-image")
import ghost_probe as probe  # noqa: E402

OUT = Path(os.environ.get("OUT", "~/.cache/miyu-copy-styles")).expanduser()
OUT.mkdir(parents=True, exist_ok=True)


def w(s):
    sys.stdout.write(s)


def main():
    w("\x1b[2J\x1b[H")
    # A. 现状:竖线装饰
    w("A 现状\r\n")
    w("\x1b[1m\x1b[34m┃\x1b[0m\r\n\x1b[1m\x1b[34m┃\x1b[0m 你好,帮我看看这段代码\r\n\x1b[1m\x1b[34m┃\x1b[0m\r\n")
    w("$ 运行命令×1 ok\r\n  \x1b[2m↳\x1b[0m ls -la\r\n\x1b[2m  │\x1b[0m \x1b[2mtotal 12\x1b[0m\r\n\x1b[2m  │\x1b[0m \x1b[2mdrwxr-xr-x  src\x1b[0m\r\n\r\n")
    # B. 回显整行淡背景(BCE 擦到行尾,不写空格),命令输出只缩进
    w("B 整行淡背景 + 纯缩进\r\n")
    w("\x1b[48;2;30;34;52m\x1b[K\r\n\x1b[48;2;30;34;52m\x1b[K 你好,帮我看看这段代码\r\n\x1b[48;2;30;34;52m\x1b[K\x1b[0m\r\n")
    w("$ 运行命令×1 ok\r\n  \x1b[2m↳\x1b[0m ls -la\r\n    \x1b[2mtotal 12\x1b[0m\r\n    \x1b[2mdrwxr-xr-x  src\x1b[0m\r\n\r\n")
    # C. 回显无背景,用颜色+加粗标识;输出块用首尾细横线
    w("C 无背景,颜色区分 + 横线围栏\r\n")
    w("\r\n\x1b[1m\x1b[34m你好,帮我看看这段代码\x1b[0m\r\n\r\n")
    w("$ 运行命令×1 ok\r\n  \x1b[2m↳\x1b[0m ls -la\r\n  \x1b[2m╭──────────────────────\x1b[0m\r\n    \x1b[2mtotal 12\x1b[0m\r\n    \x1b[2mdrwxr-xr-x  src\x1b[0m\r\n  \x1b[2m╰──────────────────────\x1b[0m\r\n\r\n")
    # D. 半格细线:U+258F 左 1/8 块(仍是字符,复制会带上,作对照)
    w("D 细线字符 ▏(对照,会被复制)\r\n")
    w("\x1b[34m▏\x1b[0m\r\n\x1b[34m▏\x1b[0m 你好,帮我看看这段代码\r\n\x1b[34m▏\x1b[0m\r\n\r\n")
    # E. 回显:首行带一个「›」标记,其余行缩进;背景只铺到文字宽度
    w("E 行首标记 › + 文字宽背景\r\n")
    w("\x1b[1m\x1b[34m›\x1b[0m \x1b[48;2;30;34;52m 你好,帮我看看这段代码 \x1b[0m\r\n\r\n")
    sys.stdout.flush()
    time.sleep(0.6)
    subprocess.run(["grim", str(OUT / "styles.png")], check=False)
    text = probe.kitten("get-text", "--extent", "screen").stdout
    (OUT / "get-text.txt").write_text(text, encoding="utf-8")
    # 选区复制:用 kitten 建一个矩形/流式选区再读 selection
    # 先选 B 的回显三行(第 12-14 行,从 1 数)
    (OUT / "done").write_text("ok")


if __name__ == "__main__":
    main()
