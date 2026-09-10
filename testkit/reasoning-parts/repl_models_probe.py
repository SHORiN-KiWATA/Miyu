#!/usr/bin/env python3
"""REPL 里 `/models …` 的输出该落在输入框上方,而不是孤零零留在输入框底下。

    BIN=target/release/miyu python3 testkit/reasoning-parts/repl_models_probe.py

复用 repro_openai 的沙箱(隔离 home + daemon + OpenAI 桩 + PTY),依次敲
`/models stub-model`(钉模型)与 `/models default`(恢复跟随全局),用 pyte 渲染屏幕,
断言两条结果行都在输入框(┃ 开头的行)之上、footer 之下没有游离文本。
"""
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
os.environ.setdefault("MODE", "plain")
import repro_openai as base  # noqa: E402

import pyte  # noqa: E402


def main():
    if base.HOME.exists():
        import shutil
        shutil.rmtree(base.HOME)
    base.RUNTIME.mkdir(parents=True, exist_ok=True)
    base.write_config()
    import subprocess
    stub = subprocess.Popen([sys.executable, str(base.HERE / "stub_reasoning.py")],
                            env=dict(os.environ, STUB_PORT=str(base.STUB_PORT), MODE="plain"),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    repl = None
    try:
        assert base.wait_http(f"http://127.0.0.1:{base.STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(base.BIN), "__daemon", "--port", str(base.PORT)], env=base.ENV, cwd=str(base.HOME),
                                  stdout=(base.OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert base.wait_http(f"http://127.0.0.1:{base.PORT}/api/config", timeout=40), "daemon not up"
        time.sleep(1)
        repl = base.Repl(base.OUT / "raw-models.bin")
        time.sleep(4)
        repl.send("/models stub-model")
        time.sleep(3)
        repl.send("/models default")
        time.sleep(3)
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
    screen = pyte.Screen(base.COLS, base.ROWS)
    pyte.ByteStream(screen).feed(bytes(repl.raw))
    lines = [line.rstrip() for line in screen.display]
    (base.OUT / "screen-models.txt").write_text("\n".join(lines))
    shown = [(i, l) for i, l in enumerate(lines) if l.strip()]
    for i, l in shown[-14:]:
        print(f"{i:3} {l}")
    footer_rows = [i for i, l in lines_enum(lines) if "普通 ·" in l or "开发 ·" in l]
    footer = footer_rows[-1] if footer_rows else len(lines)
    stray = [l for i, l in enumerate(lines) if i > footer and l.strip()]
    notes = [i for i, l in enumerate(lines) if "恢复跟随全局" in l or "当前会话模型" in l or "会话模型已更新" in l]
    # 旧版:println 的结果行被画到输入框那一行上(「┃ 1u当前会话已恢复…」),
    # 「已更新当前会话模型」那行则整个被活动区盖掉。结果行必须独占一行、
    # 不带输入框边条,两条 println 结果都得在。
    on_bar = [i for i in notes if lines[i].lstrip().startswith("┃")]
    has_follow = any("恢复跟随全局" in l for l in lines)
    has_updated = any("已更新当前会话模型" in l or "当前会话模型:" in l or "当前会话模型：" in l for l in lines)
    ok = bool(notes) and not on_bar and has_follow and has_updated and all(i < footer for i in notes) and not stray
    print("notes rows:", notes, "on input bar:", on_bar, "has_follow:", has_follow, "has_updated:", has_updated,
          "footer row:", footer, "stray below footer:", stray)
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


def lines_enum(lines):
    return list(enumerate(lines))


if __name__ == "__main__":
    sys.exit(main())
