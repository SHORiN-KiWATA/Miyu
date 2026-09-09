#!/usr/bin/env python3
"""真机 kitty(无头 cage)里复现「回车后光标瞬移到左下角」。

在 kitty 窗口内起 miyu REPL(沙箱 home + 独立端口 daemon + 桩 LLM),用 kitten 远程控制
敲入提示并回车,回车前后用 grim 连拍;逐帧统计屏幕最底一行左侧(活动区永远不占最后
一行,那格本该是纯背景)有没有亮像素——有就是光标本体或 cursor_trail 拖尾跑到了左下角。

用法(kitty 会读用户自己的 kitty.conf,里面 cursor_trail 1):
    OUT=~/.cache/miyu-trail-probe BIN=target/debug/miyu \
      testkit/kitty-image/run_headless.sh python3 <this>/trail_probe.py
产物:$OUT/frames/turn<N>-<i>.png、$OUT/verdict.json
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

from PIL import Image

REPO = Path("/home/shorin/Documents/github/Miyu")
sys.path.insert(0, str(REPO / "testkit" / "kitty-image"))
import ghost_probe as probe  # noqa: E402

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
OUT = Path(os.environ.get("OUT") or "~/.cache/miyu-trail-probe").expanduser()
TAG = os.environ.get("TAG", "run")
HOME = OUT / "home"
RUN = Path.home() / ".cache" / "miyu-tp-run"
FRAMES = OUT / f"frames-{TAG}"
PORT = int(os.environ.get("PORT", "18398"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18498"))
TURNS = int(os.environ.get("TURNS", "3"))
LINES = int(os.environ.get("LINES", "30"))
BURST_SECS = float(os.environ.get("BURST_SECS", "1.6"))
LOG = open(OUT / f"trail-{TAG}.log", "a", encoding="utf-8")

spec = importlib.util.spec_from_file_location("clitk", REPO / "testkit" / "cli" / "run.py")
clitk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(clitk)
clitk.HOME, clitk.RUN, clitk.OUT, clitk.PORT, clitk.STUB_PORT = HOME, RUN, OUT / "cli-out", PORT, STUB_PORT
clitk.MIYU = BIN


def log(msg):
    LOG.write(f"{time.strftime('%H:%M:%S')} {msg}\n")
    LOG.flush()


def env():
    e = clitk.env()
    e["TERM"] = "xterm-kitty"
    return e


def screen_text():
    return probe.kitten("get-text", "--extent", "screen").stdout


def wait_text(needle, timeout):
    t0 = time.time()
    while time.time() - t0 < timeout:
        if needle in screen_text():
            return True
        time.sleep(0.15)
    return False


def wait_stable(timeout=30.0, hold=1.2):
    last, since, start = None, time.time(), time.time()
    while time.time() - start < timeout:
        text = screen_text()
        if text != last:
            last, since = text, time.time()
        elif time.time() - since > hold and text.strip():
            return text
        time.sleep(0.15)
    return last or ""


class Burst:
    def __init__(self, prefix):
        self.prefix = prefix
        self.frames = []
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        i = 0
        while not self.stop.is_set():
            path = FRAMES / f"{self.prefix}-{i:03d}.png"
            t = time.monotonic()
            subprocess.run(["grim", str(path)], check=False, capture_output=True)
            self.frames.append((t, path))
            i += 1

    def start(self):
        self.thread.start()

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=5)
        return self.frames


def bright_pixels(path, box, threshold=110):
    img = Image.open(path).convert("RGB")
    x0, y0, x1, y1 = box
    x1, y1 = min(x1, img.width), min(y1, img.height)
    count = 0
    px = img.load()
    for y in range(y0, y1):
        for x in range(x0, x1):
            r, g, b = px[x, y]
            if max(r, g, b) > threshold:
                count += 1
    return count


def main():
    assert BIN.exists(), BIN
    OUT.mkdir(parents=True, exist_ok=True)
    if FRAMES.exists():
        shutil.rmtree(FRAMES)
    FRAMES.mkdir(parents=True)
    clitk.build_home()
    cfg_path = HOME / "config" / "config.jsonc"
    cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
    cfg["providers"][0]["base_url"] = f"http://127.0.0.1:{STUB_PORT}/v1"
    cfg_path.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")

    rows, cols, xpixel, ypixel = probe.term_geometry()
    cell_w, cell_h = max(xpixel // cols, 1), max(ypixel // rows, 1)
    log(f"geometry rows={rows} cols={cols} cell={cell_w}x{cell_h} px={xpixel}x{ypixel}")

    stub = subprocess.Popen([sys.executable, str(HERE / "stub_long.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = subprocess.Popen([str(BIN), "daemon", "--port", str(PORT)], env=env(),
                              stdout=(OUT / f"daemon-{TAG}.log").open("w"), stderr=subprocess.STDOUT)
    verdict = {"tag": TAG, "geometry": {"rows": rows, "cols": cols, "cell_w": cell_w, "cell_h": cell_h}, "turns": []}
    miyu = None
    try:
        for _ in range(60):
            if clitk.find_socket():
                break
            time.sleep(0.5)
        assert clitk.find_socket(), "daemon socket never appeared"
        time.sleep(1.0)
        log("starting repl")
        miyu = subprocess.Popen([str(BIN)], env=env(), cwd=str(OUT))
        log("kitten probe: " + repr(probe.kitten("ls").stderr[:200]))
        assert wait_text("┃", 40), "input box never appeared: " + screen_text()[-300:]
        wait_stable(20)
        # 截一张基准图,拿全图尺寸算偏移(kitty 居中 + padding)。
        base = FRAMES / "base.png"
        subprocess.run(["grim", str(base)], check=False, capture_output=True)
        img = Image.open(base)
        off_y = max((img.height - rows * cell_h) // 2, 0)
        off_x = max((img.width - cols * cell_w) // 2, 0)
        last_row_box = (0, off_y + (rows - 1) * cell_h, off_x + 4 * cell_w, img.height)
        log(f"image {img.width}x{img.height} off=({off_x},{off_y}) last_row_box={last_row_box}")
        verdict["last_row_box"] = last_row_box
        for i in range(1, TURNS + 1):
            probe.kitten("send-text", f"TK turn {i} LINES={LINES}")
            time.sleep(0.5)
            burst = Burst(f"turn{i}")
            burst.start()
            time.sleep(0.15)
            t_enter = time.monotonic()
            probe.kitten("send-key", "enter")
            time.sleep(BURST_SECS)
            frames = burst.finish()
            wait_text(f"line {LINES} of {LINES}", 60)
            wait_stable(30)
            hits = []
            for t, path in frames:
                n = bright_pixels(path, last_row_box)
                if n > 0:
                    hits.append({"t": round(t - t_enter, 3), "frame": path.name, "bright": n})
            verdict["turns"].append({"turn": i, "frames": len(frames), "hits": hits,
                                     "first_frame_dt": round(frames[0][0] - t_enter, 3) if frames else None})
            log(f"turn {i}: frames={len(frames)} hits={len(hits)} {hits[:6]}")
    finally:
        if miyu:
            probe.kitten("send-text", "/exit\r")
            try:
                miyu.wait(timeout=8)
            except subprocess.TimeoutExpired:
                miyu.kill()
        subprocess.run([str(BIN), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        stub.terminate()
        (OUT / f"verdict-{TAG}.json").write_text(json.dumps(verdict, ensure_ascii=False, indent=2), encoding="utf-8")
        log(json.dumps(verdict, ensure_ascii=False))


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        import traceback
        log(traceback.format_exc())
        raise
