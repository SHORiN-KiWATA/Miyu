#!/usr/bin/env python3
"""中间正文投递时机的真机验收(假 claude CLI,不花额度)。

支点是**时间**,不是条数。claude-code 这条线整轮只发一次请求、工具循环在
claude 侧闭环,所以修复前后"最终都发两条"是一样的——区别在第一条什么时候
到:修复前跟第二条一起卡在回合末尾,修复后应当在工具那 6 秒停顿里就到了。

判据:第一条 QQ 出站消息的到达时间 < 工具停顿结束时间。

    python3 testkit/intermediate-reply/run.py
"""

import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-imr/home"))
WORK = Path(os.environ.get("MIYU_IMR_WORK", "/tmp/miyu-imr/work"))
PORT = int(os.environ.get("MIYU_IMR_PORT", "18391"))
WS_PORT = int(os.environ.get("MIYU_IMR_WS_PORT", "18392"))
DELAY = float(os.environ.get("MIYU_FAKE_TOOL_DELAY", "6"))
RUNTIME = "/tmp/mx-imr"

ENV = dict(
    os.environ,
    MIYU_HOME=str(HOME),
    XDG_RUNTIME_DIR=RUNTIME,
    MIYU_FAKE_TOOL_DELAY=str(DELAY),
)

sys.path.insert(0, str(REPO / "testkit" / "fake-onebot"))


def build_home() -> None:
    """隔离 HOME:先让 daemon 生成一份默认配置,再打补丁。

    手搓配置会漏必填字段(第一版就栽在 `display_name` 上),而默认配置本身就
    带着 claude-code 供应商,改几个字段比从零造稳得多。
    """
    HOME.mkdir(parents=True, exist_ok=True)
    Path(RUNTIME).mkdir(exist_ok=True)
    WORK.mkdir(parents=True, exist_ok=True)
    fake = HOME / "fake-claude"
    shutil.copy(Path(__file__).parent / "fake_claude.sh", fake)
    fake.chmod(0o755)

    path = HOME / "config" / "config.jsonc"
    if not path.exists():
        daemon("start", "--port", str(PORT))
        daemon("stop")
    config = json.loads(re.sub(r"^\s*//.*$", "", path.read_text("utf-8"), flags=re.M))

    for provider in config["providers"]:
        if provider["id"] == "claude-code":
            provider["enabled"] = True
            provider["default_model"] = "haiku"
    config["active_provider"] = "claude-code"
    config["active_provider_models"] = [{"provider_id": "claude-code", "model": "haiku"}]
    config.setdefault("plugins", {})["claude_code"] = {
        "binary": str(fake),
        "native_tools": "off",
        "miyu_tools": "off",
        "permission_mode": "bypassPermissions",
        "idle_timeout_seconds": 120,
        "prefer_subscription": True,
    }
    qq = config.setdefault("platforms", {}).setdefault("qq", {})
    qq.update({
        "enabled": True,
        "reverse_ws_port": WS_PORT,
        "access_token": "",
        "private_intermediate_messages": True,
        "group_intermediate_messages": False,
    })
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def daemon(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(BIN), "daemon", *args], env=ENV, cwd=WORK,
        capture_output=True, text=True, timeout=90,
    )


def main() -> int:
    build_home()
    daemon("stop")
    started = daemon("start", "--port", str(PORT))
    print("daemon:", (started.stdout + started.stderr).strip().splitlines()[:2])
    try:
        time.sleep(4)
        import run as onebot  # testkit/fake-onebot/run.py

        onebot.HOST, onebot.PORT = "127.0.0.1", WS_PORT
        ws = onebot.WS.connect("")
        arrivals: list[tuple[float, str]] = []

        # 原函数先绑进闭包。写成 `onebot.api_data(...)` 会在替换后指回自己,
        # 无限递归——第一版就这样,把一条消息记成了几十条。
        inner = onebot.api_data

        def record(action, params):
            if action in ("send_private_msg", "send_msg", "send_group_msg"):
                text = "".join(
                    seg.get("data", {}).get("text", "")
                    for seg in params.get("message", [])
                    if isinstance(seg, dict)
                )
                arrivals.append((time.monotonic(), text))
            return inner(action, params)

        onebot.api_data = record
        import threading
        threading.Thread(target=onebot.pump, args=(ws,), daemon=True).start()
        time.sleep(1)

        sent_at = time.monotonic()
        onebot.private_msg(ws, "帮我查一下")
        deadline = sent_at + DELAY + 40
        while time.monotonic() < deadline and len(arrivals) < 2:
            time.sleep(0.2)

        tool_done_at = sent_at + DELAY
        print(f"\n发出提问 t=0.0s，工具停顿到 t={DELAY:.1f}s")
        for at, text in arrivals:
            print(f"  t={at - sent_at:5.1f}s  {text[:60]!r}")

        failures = []
        if len(arrivals) < 2:
            failures.append(f"只收到 {len(arrivals)} 条消息，期望 2 条（中间 + 最终）")
        elif arrivals[0][0] >= tool_done_at:
            failures.append(
                f"第一条在 t={arrivals[0][0] - sent_at:.1f}s 才到，"
                f"晚于工具停顿结束 t={DELAY:.1f}s —— 还是攒到最后一起发的"
            )
        if arrivals and "第一段" not in arrivals[0][1]:
            failures.append(f"第一条内容不是第一段正文：{arrivals[0][1]!r}")
        if len(arrivals) >= 2 and "第二段" not in arrivals[1][1]:
            failures.append(f"第二条内容不是第二段正文：{arrivals[1][1]!r}")
        if len(arrivals) >= 2 and "第一段" in arrivals[1][1]:
            failures.append("第二条把第一段又发了一遍（flush 后没清 text）")

        for line in failures:
            print("FAIL " + line)
        print("\n全过" if not failures else f"\n{len(failures)} 项未过")
        return 1 if failures else 0
    finally:
        daemon("stop")


if __name__ == "__main__":
    sys.exit(main())
