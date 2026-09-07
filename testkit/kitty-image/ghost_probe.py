#!/usr/bin/env python3
"""复现并判定「滚上去看历史后新输出把图片切片一条条往下复制」的残影。

机制(kitty 0.48 源码 screen.c / graphics.c):

- Unicode 占位符图片不是按坐标钉在屏上的,而是渲染每一帧时扫描可见行,
  给每个占位符行生成一条一行高的"cell image"引用(`screen_render_line_graphics`)。
- 屏内的行只在脏了才重扫;**历史区的行每一帧都重扫**,扫之前只清掉
  「本行当前 row」上的旧引用。
- 正文用 DECSTBM 受限区滚动时,kitty 只搬「完全落在页内」的引用
  (`scroll_filter_margins_func`),历史区那些 row 为负的引用一律不动。
- 用户滚上去看历史时 scrolled_by>0,每滚一行 scrolled_by 就 +1 好让视口
  钉住;历史行下一帧在 row-1 重扫,旧 row 上那条引用没人清、又没被搬,
  就以 (row + scrolled_by) 画在比原位低一行的地方——每滚一次多一条。
- 整屏滚动(无页边距)走 `scroll_filter_func`,负 row 的引用一起搬,没有残影。

本探针在同一个窗口里连做两轮:

    A. 受限区滚动(Miyu 现状)  → 预期留残影
    B. 推入历史仍用受限区,之后改整屏滚 + 插入行把活动区推回原位 → 预期不再新增
    C. 全程整屏滚 + 插入行(候选修法落地后的样子) → 预期全程干净

A 里"滚之前"就会有一条残影:kitty 默认 pixel_scroll=yes,每帧会多渲染视口上方
一行(历史行 -1)以便平滑滚动,图片底行刚进历史时就在那一行被扫成 row=-1 的引用,
下一行推进历史后没人清它——这就是用户看到的"图片底部切片"。

每轮:画一张蓝块,把它滚进历史,用 kitty 远程控制把视口滚上去让它露出来,
再滚正文 SCROLL_BY 次(每次隔一帧),然后 grim 截图。判定用像素:统计
每个单元格行里的蓝色像素,蓝块本该只占 IMAGE_ROWS 行,多出来的行就是残影。

必须在真实 kitty 里跑,且 kitty 要带 `--listen-on` 好让探针能滚视口;
`run_headless.sh` 会在无头 cage 里把这一切包好。

用法(每个变体单独一个 kitty 进程,残影引用留下后没人清,会污染下一轮):
    OUT=~/.cache/miyu-kitty-probe run_headless.sh python3 ghost_probe.py A
    OUT=~/.cache/miyu-kitty-probe run_headless.sh python3 ghost_probe.py B
产物:$OUT/{A,B}-{before,after}.png 和 $OUT/verdict-{A,B}.json
"""
import base64
import fcntl
import json
import os
import struct
import subprocess
import sys
import termios
import time

PLACEHOLDER = "\U0010eeee"
ROW_DIACRITICS = ["̅", "̍", "̎", "̐", "̒", "̽", "̾", "̿"]
IMAGE_ROWS = 4
IMAGE_COLS = 12
TAIL_ROWS = 6
SCROLL_BY = 6
BLUE = (0x1E, 0x64, 0xC8)
OUT = os.environ.get("OUT") or os.path.expanduser("~/.cache/miyu-kitty-probe")


def out(text):
    sys.stdout.write(text)


def flush():
    sys.stdout.flush()


def at(row, text=""):
    out(f"\x1b[{row + 1};1H\x1b[K{text}")


def term_geometry():
    packed = fcntl.ioctl(sys.stdout.fileno(), termios.TIOCGWINSZ, b"\0" * 8)
    rows, cols, xpixel, ypixel = struct.unpack("HHHH", packed)
    return rows, cols, xpixel, ypixel


def draw_image(top, cell_w, cell_h, image_id):
    width, height = IMAGE_COLS * cell_w, IMAGE_ROWS * cell_h
    raw = bytes(BLUE + (0xFF,)) * (width * height)
    encoded = base64.standard_b64encode(raw)
    chunks = [encoded[i:i + 4096] for i in range(0, len(encoded), 4096)]
    for index, chunk in enumerate(chunks):
        more = 1 if index + 1 < len(chunks) else 0
        head = (
            f"\x1b_Gq=2,i={image_id},a=T,U=1,f=32,t=d,"
            f"s={width},v={height},c={IMAGE_COLS},r={IMAGE_ROWS},m={more};"
            if index == 0
            else f"\x1b_Gq=2,m={more};"
        )
        out(head + chunk.decode("ascii") + "\x1b\\")
    red, green, blue = (image_id >> 16) & 0xFF, (image_id >> 8) & 0xFF, image_id & 0xFF
    for row in range(IMAGE_ROWS):
        at(top + row)
        out(f"\x1b[38;2;{red};{green};{blue}m")
        for col in range(IMAGE_COLS):
            out(PLACEHOLDER + ROW_DIACRITICS[row] + ROW_DIACRITICS[col % len(ROW_DIACRITICS)])
        out(f"\x1b[39m ←── 蓝块第 {row + 1} 行")


def draw_tail(rows):
    for row in range(rows - TAIL_ROWS, rows):
        at(row, f"┃ 活动区 第 {row} 行 ┃")


def scroll_region_once(region_bottom_1based):
    """Miyu 现状:DECSTBM 受限区 + 在区底换行。"""
    out(f"\x1b[1;{region_bottom_1based}r\x1b[{region_bottom_1based};1H\n\x1b[r")


def scroll_fullscreen_once(rows, region_bottom_1based):
    """候选修法:整屏滚一行(无页边距),再在区底下一行插入一行把活动区推回去。"""
    out(f"\x1b[{rows};1H\n")
    out(f"\x1b[{region_bottom_1based};1H\x1b[1L")


def kitten(*args):
    listen = os.environ.get("KITTY_LISTEN_ON")
    cmd = ["kitten", "@"]
    if listen:
        cmd += ["--to", listen]
    cmd += list(args)
    return subprocess.run(cmd, capture_output=True, text=True)


def scroll_viewport_up(lines):
    for _ in range(lines):
        kitten("action", "scroll_line_up")
        time.sleep(0.02)


def screenshot(name):
    path = os.path.join(OUT, name + ".png")
    time.sleep(0.25)
    subprocess.run(["grim", path], check=False)
    return path


def blue_rows(path, cell_w, cell_h, rows, cols):
    """每个单元格行里的蓝色像素数;超过四分之一条切片就算这一行有蓝块。

    kitty 默认 placement_strategy=center,格子区在窗口里居中,得先扣掉上下
    左右的留白,否则每个格子行都跨两个真实行,4 行的图会数出 5 行。
    """
    from PIL import Image

    image = Image.open(path).convert("RGB")
    width, height = image.size
    offset_y = max((height - rows * cell_h) // 2, 0)
    offset_x = max((width - cols * cell_w) // 2, 0)
    pixels = image.load()
    counts = []
    scan_w = min(width, offset_x + (IMAGE_COLS + 2) * cell_w)
    for row in range(rows):
        top = offset_y + row * cell_h
        bottom = min(top + cell_h, height)
        count = 0
        for y in range(top, bottom):
            for x in range(offset_x, scan_w):
                r, g, b = pixels[x, y]
                if abs(r - BLUE[0]) < 12 and abs(g - BLUE[1]) < 12 and abs(b - BLUE[2]) < 12:
                    count += 1
        counts.append(count)
    threshold = IMAGE_COLS * cell_w * cell_h // 4
    return [row for row, count in enumerate(counts) if count > threshold]


def run_variant(kind, rows, cols, cell_w, cell_h, image_id, verdict):
    region_bottom = rows - TAIL_ROWS  # 1-based 区底 = 0-based 活动区首行
    out("\x1b[2J\x1b[H")
    filler_above = 3
    for row in range(filler_above):
        at(row, f"[{kind}] 填充 {row}")
    image_top = filler_above
    draw_image(image_top, cell_w, cell_h, image_id)
    for row in range(image_top + IMAGE_ROWS, region_bottom):
        at(row, f"[{kind}] 填充 {row}")
    draw_tail(rows)
    flush()
    time.sleep(0.3)

    # 把蓝块整个滚进历史:滚 (image_top + IMAGE_ROWS + 2) 行,视口仍在底部。
    push = image_top + IMAGE_ROWS + 2
    for i in range(push):
        if kind == "C":
            scroll_fullscreen_once(rows, region_bottom)
        else:
            scroll_region_once(region_bottom)
        at(region_bottom - 1, f"[{kind}] 推入历史 {i}")
        flush()
        time.sleep(0.03)

    # 用户滚上去看历史:让蓝块露出在视口上部。
    view_up = push + 2
    scroll_viewport_up(view_up)
    before = screenshot(f"{kind}-before")
    rows_before = blue_rows(before, cell_w, cell_h, rows, cols)

    # AI 继续输出:正文再滚 SCROLL_BY 次,每次隔一帧。
    for i in range(SCROLL_BY):
        if kind == "A":
            scroll_region_once(region_bottom)
        else:
            scroll_fullscreen_once(rows, region_bottom)
        at(region_bottom - 1, f"[{kind}] 新输出 {i}")
        flush()
        time.sleep(0.08)
    after = screenshot(f"{kind}-after")
    rows_after = blue_rows(after, cell_w, cell_h, rows, cols)

    # 回到底部,清掉这一轮的状态,给下一轮让路。
    kitten("action", "scroll_end")
    time.sleep(0.1)
    verdict[kind] = {
        "blue_rows_before": rows_before,
        "blue_rows_after": rows_after,
        "expected_rows": IMAGE_ROWS,
        "ghost_rows": max(0, len(rows_after) - IMAGE_ROWS),
    }


def main():
    if os.environ.get("TERM") != "xterm-kitty":
        print(f"TERM={os.environ.get('TERM')!r},必须在真实 kitty 里跑。")
        return 1
    os.makedirs(OUT, exist_ok=True)
    rows, cols, xpixel, ypixel = term_geometry()
    cell_w, cell_h = max(xpixel // cols, 1), max(ypixel // rows, 1)
    verdict = {"geometry": {"rows": rows, "cols": cols, "cell_w": cell_w, "cell_h": cell_h}}
    # 每个变体单独一个 kitty:残影引用一旦留下就没人清,会污染下一轮的判定。
    kind = (sys.argv[1:] or ["A"])[0]
    run_variant(kind, rows, cols, cell_w, cell_h, 0x001E64C8, verdict)
    with open(os.path.join(OUT, f"verdict-{kind}.json"), "w", encoding="utf-8") as file:
        json.dump(verdict, file, ensure_ascii=False, indent=2)
    out("\x1b[2J\x1b[H")
    print(json.dumps(verdict, ensure_ascii=False, indent=2))
    flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
