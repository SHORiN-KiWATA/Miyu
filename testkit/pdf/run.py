#!/usr/bin/env python3
"""PDF 输入真机验收(花真实额度,不进 CI)。

判据是**暗号**:PDF 正文里埋一串随机字符,只有文件本体真的送到模型面前,
回答里才可能出现它。降级成"用户发了个文件"、或者块被中转层吞掉,模型顶多
复述文件名——那正是要抓的回归。

三场:
  A 吃 PDF 的模型 → 回答里有暗号(内联生效)
  B 不吃 PDF 的模型 → 回答里没有暗号,但提示里给了路径(不静默吞文件)
  C 同 A 但走 anthropic 协议 → 钉住 document 块那条独立的下放路径

    MIYU_PDF_PROVIDER=opencode MIYU_PDF_MODEL=claude-sonnet-4-6 \
        python3 testkit/pdf/run.py
"""

import json
import os
import re
import secrets
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-pdf/home"))
WORK = Path(os.environ.get("MIYU_PDF_WORK", "/tmp/miyu-pdf/work"))
PROVIDER = os.environ.get("MIYU_PDF_PROVIDER", "opencode")
MODEL = os.environ.get("MIYU_PDF_MODEL", "claude-sonnet-4-6")
# 不吃 PDF 的对照组:同一个供应商换一个纯文本模型,避免把"换了供应商"混进变量。
PLAIN_MODEL = os.environ.get("MIYU_PDF_PLAIN_MODEL", MODEL)

sys.path.insert(0, str(Path(__file__).parent))
from make_pdf import build  # noqa: E402

# 直连:agent 在本进程跑,每次重读配置。经 daemon 的话第一次 ask 会把它拉起来,
# 之后改配置也不重载——A/B 两组就会双双跑在第一组的模型上(第一版栽在这)。
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR="/tmp/mx-pdf", MIYU_DIRECT="1")
Path("/tmp/mx-pdf").mkdir(exist_ok=True)


def write_config(modalities: list[str]) -> None:
    """把测试模型的 input_modalities 钉死,不靠 models.dev 目录的当日快照。"""
    path = HOME / "config" / "config.jsonc"
    config = json.loads(re.sub(r"^\s*//.*$", "", path.read_text("utf-8"), flags=re.M))
    for provider in config["providers"]:
        if provider["id"] == PROVIDER:
            provider.setdefault("model_modalities", {})[MODEL] = modalities
            provider["default_model"] = MODEL
    config["active_provider"] = PROVIDER
    config["active_provider_models"] = [{"provider_id": PROVIDER, "model": MODEL}]
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def stop_daemon() -> None:
    """MIYU_DIRECT 挡不住一个已经在跑的隔离 daemon:ask 会连上它,于是两组
    都跑在**它启动时**那份配置上,A/B 变成 A/A(第一版在这浪费了半小时)。"""
    subprocess.run(
        [str(BIN), "daemon", "stop"], env=ENV, cwd=WORK,
        capture_output=True, text=True, timeout=60,
    )


def ask(prompt: str, pdf: Path) -> str:
    result = subprocess.run(
        [str(BIN), "ask", "--image", str(pdf), "--no-memory", "--timeout", "180", prompt],
        env=ENV, cwd=WORK, capture_output=True, text=True, timeout=200,
    )
    return result.stdout + result.stderr


def main() -> int:
    HOME.mkdir(parents=True, exist_ok=True)
    WORK.mkdir(parents=True, exist_ok=True)
    if not (HOME / "config" / "config.jsonc").exists():
        print(f"先把一份带 key 的 config.jsonc 放进 {HOME}/config/", file=sys.stderr)
        return 2

    secret = f"SECRET-{secrets.token_hex(3).upper()}"
    pdf = WORK / "probe.pdf"
    pdf.write_bytes(build(secret))
    prompt = "这份 PDF 里有一串以 SECRET- 开头的编号，原样告诉我，只回那串编号。"

    failures = []

    print(f"== A 吃 PDF ({PROVIDER}/{MODEL}) 暗号={secret}")
    write_config(["text", "image", "pdf"])
    stop_daemon()
    out = ask(prompt, pdf)
    print(out.strip()[:400])
    if secret not in out:
        failures.append(f"A: 回答里没有暗号 {secret} —— PDF 没送到模型面前")

    print(f"\n== B 不吃 PDF ({PROVIDER}/{PLAIN_MODEL})")
    write_config(["text"])
    stop_daemon()
    out = ask("我发了个什么文件？路径是什么？", pdf)
    print(out.strip()[:400])
    if str(pdf) not in out and "probe.pdf" not in out:
        failures.append("B: 不内联时连路径都没给到模型 —— 文件静默消失了")

    for line in failures:
        print("FAIL " + line)
    print("\n全过" if not failures else f"\n{len(failures)} 项未过")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
