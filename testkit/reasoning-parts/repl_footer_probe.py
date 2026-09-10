#!/usr/bin/env python3
"""REPL 里 `/models <型号>` 之后,footer 的模型标签要**立刻**换,不能等下一次按键。

    BIN=target/release/miyu python3 testkit/reasoning-parts/repl_footer_probe.py

沙箱同 repro_openai(隔离 home + daemon + OpenAI 桩 + PTY),但配两个型号:
全局 stub-model,敲 `/models stub-b` 后不按任何键,直接把 PTY 收到的字节渲染成屏,
断言最后一行 footer(「普通 ·」那行)里已经是 stub-b。修前:仍是 stub-model,
再敲一个字符才换(09-10 用户截图)。
"""
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
os.environ.setdefault("MODE", "plain")
import repro_openai as base  # noqa: E402

import pyte  # noqa: E402


def write_config():
    (base.HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{"id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{base.STUB_PORT}/v1",
                       "protocol": "openai-chat", "api_key": "stub", "models": ["stub-model", "stub-b"]}],
        "memory": {"enabled": False},
    }
    (base.HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def render(raw):
    screen = pyte.Screen(base.COLS, base.ROWS)
    pyte.ByteStream(screen).feed(bytes(raw))
    return [line.rstrip() for line in screen.display]


def footer_of(lines):
    rows = [l for l in lines if "普通 ·" in l or "开发 ·" in l]
    return rows[-1] if rows else ""


def main():
    if base.HOME.exists():
        shutil.rmtree(base.HOME)
    base.RUNTIME.mkdir(parents=True, exist_ok=True)
    write_config()
    stub = subprocess.Popen([sys.executable, str(base.HERE / "stub_reasoning.py")],
                            env=dict(os.environ, STUB_PORT=str(base.STUB_PORT), MODE="plain"),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = repl = None
    before = after_switch = after_key = None
    try:
        assert base.wait_http(f"http://127.0.0.1:{base.STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(base.BIN), "__daemon", "--port", str(base.PORT)], env=base.ENV, cwd=str(base.HOME),
                                  stdout=(base.OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert base.wait_http(f"http://127.0.0.1:{base.PORT}/api/config", timeout=40), "daemon not up"
        time.sleep(1)
        repl = base.Repl(base.OUT / "raw-footer.bin")
        time.sleep(4)
        before = footer_of(render(repl.raw))
        repl.send("/models stub-b")
        time.sleep(4)
        after_switch = footer_of(render(repl.raw))
        os.write(repl.master, b"j")
        time.sleep(1.5)
        after_key = footer_of(render(repl.raw))
    finally:
        if repl:
            repl.close()
        for p in (daemon, stub):
            if p:
                p.terminate()
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    p.kill()
    print("before      :", before)
    print("after switch:", after_switch)
    print("after key   :", after_key)
    ok = "stub-model" in (before or "") and "stub-b" in (after_switch or "")
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
