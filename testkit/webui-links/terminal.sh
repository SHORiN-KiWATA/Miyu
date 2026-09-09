#!/usr/bin/env bash
#
# 终端侧（REPL / shellhook）的链接渲染走查。
#
# 和 run.py 共用同一个桩模型：链接的判定规则两端是同一套（裸地址的句尾修剪、
# 「标题 (地址)」整行成链、file:// 也算链接），拿同一段正文去量，两边给出不同
# 结果时一眼看得见。
#
# 不花额度。沙箱 MIYU_HOME + 独立 XDG_RUNTIME_DIR，碰不到生产 daemon
# （AGENTS §5.4：普通 CLI 的未知子命令会把参数当对话发给生产 daemon）。
#
#     bash testkit/webui-links/terminal.sh
#
# 前置：cargo build（渲染代码编进二进制）。
set -u

REPO=$(cd "$(dirname "$0")/../.." && pwd)
export MIYU_HOME=${MIYU_HOME:-/tmp/miyu-term-links/home}
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR_OVERRIDE:-/tmp/mx-term}
# 真 PTY 里跑：OSC 8 的能力判定看 TERM，dumb/linux 下是**故意**不发的。
export TERM=xterm-256color
BIN=${MIYU_BIN:-$REPO/target/debug/miyu}
STUB_PORT=${STUB_PORT:-18497}
OUT=/tmp/miyu-term-links/out.raw

rm -rf /tmp/miyu-term-links "$XDG_RUNTIME_DIR"
mkdir -p "$MIYU_HOME/config" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

cat > "$MIYU_HOME/config/config.jsonc" <<JSON
{
  "active_provider": "stub",
  "active_provider_models": [{ "provider_id": "stub", "model": "stub-model" }],
  "providers": [{
    "id": "stub",
    "display_name": "Stub",
    "base_url": "http://127.0.0.1:${STUB_PORT}/v1",
    "protocol": "openai-chat",
    "api_key": "stub",
    "models": ["stub-model"]
  }],
  "memory": { "enabled": false },
  "tools": { "enabled": false }
}
JSON

STUB_PORT=$STUB_PORT python3 "$REPO/testkit/webui-links/stub_llm.py" &
STUB=$!
trap 'kill $STUB 2>/dev/null' EXIT
sleep 1

cd "$REPO"
# LINKTEST 是桩认「主回合」的暗号，别的旁路请求它只回 ok。
script -qec "$BIN 'LINKTEST 给我几个链接'" /dev/null > "$OUT" 2>&1

python3 - "$OUT" <<'PY'
import re, sys

raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
fails = []


def check(name, ok, detail=""):
    print(("  ok " if ok else "FAIL ") + name + (f" — {detail}" if detail else ""))
    if not ok:
        fails.append(name)


LINK_LABEL = "\x1b[38;5;117m"
URL = "\x1b[2m\x1b[38;5;75m"
INLINE_CODE = "\x1b[36m"
RESET = "\x1b[0m"

targets = [url for url in re.findall(r"\x1b\]8;;([^\x1b\x07]*)\x1b\\", raw) if url]
# 颜色和它后面的字之间可能隔着一个 OSC 8 收尾序列，量「有没有吃标点」时先去掉。
plain = re.sub(r"\x1b\]8;;[^\x1b\x07]*(?:\x1b\\|\x07)", "", raw)

check("发了 OSC 8 超链接", bool(targets),
      f"{len(targets)} 条：{' '.join(sorted(set(targets))[:5])}")
check("裸地址上了链接色", f"{URL}https://www.protondb.com" in raw)
check("句尾句号没被吃进地址", f"{URL}https://example.org/trailing{RESET}。" in plain)
check("顿号没被吃进地址", f"{URL}https://a.org{RESET}、" in plain or "、AUR" in plain)
check("「标题 (地址)」的标题上了链接色",
      f"{LINK_LABEL}Efficient LLM Collaboration via Planning{RESET}" in raw)
check("标题和地址挂同一个 OSC 8 目标",
      any(url.startswith("https://arxiv.org/html/2506.11578v3") for url in targets),
      " ".join(sorted(set(targets))))
check("file:// 也成链", any(url.startswith("file:///home/mac/.miyu") for url in targets))
check("md 链接的原文没漏出来", "](https://" not in raw and "](file://" not in raw)
check("行内代码里的地址没被上链接色",
      f"{INLINE_CODE}https://example.com/inside-code{RESET}" in raw)

print()
print("全过" if not fails else f"{len(fails)} 项没过：" + "、".join(fails))
sys.exit(1 if fails else 0)
PY
