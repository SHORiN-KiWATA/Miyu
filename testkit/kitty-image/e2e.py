#!/usr/bin/env python3
"""Miyu 级真机验收:在无头 kitty 里跑真 REPL,打图后滚上去看历史,新输出会不会留残影。

流程(全部在 kitty 窗口里由本脚本驱动,不碰用户桌面):

    1. 隔离 MIYU_HOME(拷真实 config,供应商换成本地桩 LLM,记忆关掉)
    2. 起桩 LLM(stub_llm.py):先 load_tools、再 print_image、再逐行慢速流式正文
    3. 直连模式起 REPL($BIN,默认本工作树的 debug 构建),用 kitten 远程控制敲入提示
    4. 桩开始流正文后,把视口往上滚 $VIEW_UP 行(模拟用户鼠标滚上去看历史)
    5. 正文流完,grim 截图,按像素统计蓝色行:图片本该只占 4 行,多的就是残影

用法:
    BIN=/usr/bin/miyu   testkit/kitty-image/run_headless.sh python3 testkit/kitty-image/e2e.py   # 对照组(旧版)
    BIN=target/debug/miyu testkit/kitty-image/run_headless.sh python3 testkit/kitty-image/e2e.py # 修复后
产物:$OUT/e2e-<tag>-{ready,streaming,after}.png、$OUT/e2e-<tag>.json
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(REPO / "testkit" / "persona-ab"))
import ghost_probe as probe  # noqa: E402
from run import strip_jsonc  # noqa: E402

OUT = Path(os.environ.get("OUT") or "~/.cache/miyu-kitty-probe").expanduser()
BIN = os.environ.get("BIN") or str(REPO / "target" / "debug" / "miyu")
TAG = os.environ.get("TAG") or ("release" if BIN.startswith("/usr") else "fixed")
HOME = OUT / "home"
RUN_DIR = OUT / "run"
STUB_PORT = int(os.environ.get("STUB_PORT", "18493"))
STUB_LOG = OUT / f"stub-{TAG}.jsonl"
IMAGE = OUT / "blue.png"
IMAGE_COLS, IMAGE_ROWS = 24, 4
VIEW_UP = int(os.environ.get("VIEW_UP", "12"))
PROMPT = "STUB_SCRIPT 请把那张蓝图显示出来然后接着说"
REAL_CONFIG = Path.home() / ".miyu" / "config" / "config.jsonc"
REAL_MODELS_CACHE = Path.home() / ".miyu" / "cache" / "models_cache.json"
LOG = open(OUT / f"e2e-{TAG}.log", "a", encoding="utf-8")


def log(msg):
    LOG.write(f"{time.strftime('%H:%M:%S')} {msg}\n")
    LOG.flush()


def build_home():
    for path in (HOME, RUN_DIR):
        if path.exists():
            shutil.rmtree(path)
    (HOME / "config").mkdir(parents=True)
    RUN_DIR.mkdir(parents=True)
    cfg = json.loads(strip_jsonc(REAL_CONFIG.read_text()), strict=False)
    for key in ("platforms", "web", "voice", "alarm"):
        cfg.pop(key, None)
    cfg["providers"] = [{
        "enabled": True,
        "id": "stub",
        "display_name": "Stub",
        "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
        "protocol": "openai-chat",
        "api_key": "stub-key",
        "models": ["stub-model"],
    }]
    cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-model"}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.setdefault("memory", {})["enabled"] = False
    cfg.setdefault("cache", {})["request_log"] = False
    (HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2))
    if REAL_MODELS_CACHE.exists():
        (HOME / "cache").mkdir(parents=True, exist_ok=True)
        shutil.copy(REAL_MODELS_CACHE, HOME / "cache" / "models_cache.json")


def make_image(cell_w, cell_h):
    from PIL import Image

    Image.new("RGB", (IMAGE_COLS * cell_w, IMAGE_ROWS * cell_h), probe.BLUE).save(IMAGE)


def env_for():
    env = dict(os.environ)
    for k in ("XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
        env.pop(k, None)
    env["MIYU_HOME"] = str(HOME)
    env["XDG_RUNTIME_DIR"] = str(RUN_DIR)
    env["MIYU_DIRECT"] = "1"
    return env


def screen_text():
    result = probe.kitten("get-text", "--extent", "screen")
    return result.stdout


def wait_stable(timeout=30.0):
    """等屏幕文字连续 1.5 秒不变(REPL 起好、输入框画完)。"""
    last, since, start = None, time.time(), time.time()
    while time.time() - start < timeout:
        text = screen_text()
        if text != last:
            last, since = text, time.time()
        elif time.time() - since > 1.5 and text.strip():
            return text
        time.sleep(0.2)
    return last or ""


def stub_events():
    if not STUB_LOG.exists():
        return []
    return [json.loads(line) for line in STUB_LOG.read_text().splitlines() if line.strip()]


def wait_stage(event, stage, timeout=60.0):
    start = time.time()
    while time.time() - start < timeout:
        for item in stub_events():
            if item["event"] == event and item["stage"] == stage:
                return True
        time.sleep(0.1)
    return False


def blue_rows(path, cell_w, cell_h, rows, cols):
    probe.IMAGE_COLS = IMAGE_COLS
    return probe.blue_rows(str(path), cell_w, cell_h, rows, cols)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    if STUB_LOG.exists():
        STUB_LOG.unlink()
    rows, cols, xpixel, ypixel = probe.term_geometry()
    cell_w, cell_h = max(xpixel // cols, 1), max(ypixel // rows, 1)
    log(f"bin={BIN} tag={TAG} term={cols}x{rows} cell={cell_w}x{cell_h}")
    build_home()
    make_image(cell_w, cell_h)
    stub_env = dict(os.environ, STUB_PORT=str(STUB_PORT), STUB_LOG=str(STUB_LOG), STUB_IMAGE=str(IMAGE),
                    STUB_IMAGE_SIZE=f"{IMAGE_COLS}x{IMAGE_ROWS}")
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_llm.py")], env=stub_env,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    verdict = {"bin": BIN, "tag": TAG, "geometry": {"rows": rows, "cols": cols, "cell_w": cell_w, "cell_h": cell_h}}
    miyu = None
    try:
        time.sleep(0.5)
        miyu = subprocess.Popen([BIN], env=env_for(), cwd=str(OUT))
        ready = wait_stable()
        log("ready screen:\n" + ready[-600:])
        probe.screenshot(f"e2e-{TAG}-ready")
        probe.kitten("send-text", PROMPT)
        time.sleep(0.2)
        probe.kitten("send-key", "enter")
        if not wait_stage("start", "text", timeout=90):
            verdict["error"] = "桩没有走到正文阶段"
            log("stub events: " + json.dumps(stub_events(), ensure_ascii=False))
            return verdict
        # 让前几行先落屏,再模拟用户滚上去看历史。
        time.sleep(0.8)
        streaming = probe.screenshot(f"e2e-{TAG}-streaming")
        verdict["blue_rows_streaming"] = blue_rows(streaming, cell_w, cell_h, rows, cols)
        probe.scroll_viewport_up(VIEW_UP)
        scrolled = probe.screenshot(f"e2e-{TAG}-scrolled")
        verdict["blue_rows_scrolled"] = blue_rows(scrolled, cell_w, cell_h, rows, cols)
        wait_stage("end", "text", timeout=90)
        time.sleep(0.6)
        after = probe.screenshot(f"e2e-{TAG}-after")
        verdict["blue_rows_after"] = blue_rows(after, cell_w, cell_h, rows, cols)
        verdict["ghost_rows_after"] = max(0, len(verdict["blue_rows_after"]) - IMAGE_ROWS)
        verdict["stages"] = [f"{e['event']}:{e['stage']}" for e in stub_events()]
        probe.kitten("action", "scroll_end")
        time.sleep(0.3)
        bottom = probe.screenshot(f"e2e-{TAG}-bottom")
        verdict["blue_rows_bottom"] = blue_rows(bottom, cell_w, cell_h, rows, cols)
        log("final screen:\n" + screen_text()[-1200:])
        return verdict
    finally:
        if miyu and miyu.poll() is None:
            miyu.send_signal(signal.SIGTERM)
            try:
                miyu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                miyu.kill()
        stub.terminate()
        (OUT / f"e2e-{TAG}.json").write_text(json.dumps(verdict, ensure_ascii=False, indent=2))
        log("verdict: " + json.dumps(verdict, ensure_ascii=False))


if __name__ == "__main__":
    main()
