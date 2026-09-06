#!/usr/bin/env python3
"""claude-code 中转线「准备xx」终端真机验证(花真实订阅额度,不进 CI)。

隔离 home + MIYU_DIRECT 直连 REPL(PTY),让 claude 用原生 Bash/Write 干活,
然后在 PTY 原始字节里找 spinner 写出的「准备执行 / 准备编辑」。

    MIYU_HOME=/tmp/miyu-ccprep/home python3 testkit/claude-code/preparing_pty.py

前提:MIYU_HOME 下 config.jsonc 激活 claude-code 供应商(建议
plugins.claude_code.miyu_tools=off,只走原生工具,少一条桥的变量);
XDG_RUNTIME_DIR 建议同时隔离。09-06 实测(haiku):准备执行 ×11 tick、
准备编辑 ×123 tick(Write 一个 60 行文件的窗口整段被盖住)。
"""

import os
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
os.environ.setdefault("MIYU_HOME", "/tmp/miyu-ccprep/home")
os.environ.setdefault("XDG_RUNTIME_DIR", "/tmp/mx-ccprep")
Path(os.environ["XDG_RUNTIME_DIR"]).mkdir(exist_ok=True)

import run as harness  # noqa: E402

HOME = Path(os.environ["MIYU_HOME"])
WORK = Path(os.environ.get("MIYU_PROBE_WORK", "/tmp/miyu-ccprep/work"))
WORK.mkdir(parents=True, exist_ok=True)
os.chdir(WORK)

PROMPT = (
    "测试任务,不要提问直接做:先用 Bash 运行 `ls -la`;"
    "然后用 Write 工具把一个约 60 行、带详细中文注释的 Rust 斐波那契程序写到 fib.rs;"
    "最后用 Bash 运行 `wc -l fib.rs`。做完只回一个字:好。"
)


def main():
    log = HOME / "cache" / "logs" / "cc-preparing-pty.log"
    repl = harness.Repl(HOME, log)
    try:
        time.sleep(4)
        start = repl.completed_turns()
        repl.send(PROMPT)
        repl.wait_turns(start + 1, timeout=240)
        time.sleep(1)
    finally:
        repl.close()
    raw = log.read_bytes().decode("utf-8", "replace")
    plain = re.sub(r"\x1b\[[0-9;?]*[a-zA-Z]", "", raw)
    counts = {k: plain.count(k) for k in ["准备执行", "准备编辑", "准备工具", "准备问题"]}
    print("phrase counts:", counts)
    for key in ["准备执行", "准备编辑"]:
        i = plain.find(key)
        if i >= 0:
            print(f"first {key!r} context:", repr(plain[max(0, i - 40) : i + 40]))
    ok = counts["准备执行"] > 0 or counts["准备编辑"] > 0
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
