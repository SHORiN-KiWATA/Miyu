#!/usr/bin/env python3
"""中转线压缩真机验收(09-10):隔离 MIYU_HOME + 真 claude-code,dev 会话用原生
Read/Edit/Write 碰文件,再 `miyu compact`,查三处:

  1. turns.tool_footprint —— 文件轮应有 read/modified
  2. 摘要行尾部 —— 应带 <read-files> / <modified-files>
  3. compact_extras.restored —— 改过的文件应被回灌(含正文)

改前(1a8bf6e3)三处全空:中转轮走 RemoteToolStarted/Finished 折成 remote 轮,
footprint 与回灌候选都看不见它。

用法:
  BIN=target/release/miyu MODEL=haiku python3 testkit/relay-compact/relay_probe.py

会消耗真实 claude-code 额度(haiku 四轮 + 一次压缩,每轮 prompt 1.5–5 万 tok,
其中九成以上是缓存读)。需要 ~/.miyu/config/config.jsonc 里有 claude-code 供应商。
"""
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
# 子进程 cwd 是工作目录,BIN 给相对路径也要先解析成绝对路径。
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "release" / "miyu").resolve()
BASE = Path(os.environ.get("PROBE_DIR") or tempfile.mkdtemp(prefix="miyu-relay-probe-"))
HOME = BASE / "home"
RUN = BASE / "run"
WORK = BASE / "work"
REAL_CONFIG = Path.home() / ".miyu" / "config" / "config.jsonc"
MODEL = os.environ.get("MODEL", "haiku")


def strip_jsonc(raw: str) -> str:
    out, in_str, esc, i = [], False, False, 0
    while i < len(raw):
        c = raw[i]
        if in_str:
            out.append(c)
            if esc:
                esc = False
            elif c == "\\":
                esc = True
            elif c == '"':
                in_str = False
            i += 1
            continue
        if c == '"':
            in_str = True
            out.append(c)
            i += 1
            continue
        if c == "/" and i + 1 < len(raw) and raw[i + 1] == "/":
            while i < len(raw) and raw[i] != "\n":
                i += 1
            continue
        if c == "/" and i + 1 < len(raw) and raw[i + 1] == "*":
            i += 2
            while i + 1 < len(raw) and not (raw[i] == "*" and raw[i + 1] == "/"):
                i += 1
            i += 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def build_home():
    for path in (HOME, RUN, WORK):
        if path.exists():
            shutil.rmtree(path)
    (HOME / "config").mkdir(parents=True)
    RUN.mkdir(parents=True)
    WORK.mkdir(parents=True)
    cfg = json.loads(strip_jsonc(REAL_CONFIG.read_text(encoding="utf-8")), strict=False)
    for key in ("platforms", "voice", "alarm"):
        cfg.pop(key, None)
    cfg["providers"] = [p for p in cfg["providers"] if p.get("id") == "claude-code"]
    assert cfg["providers"], "real config has no claude-code provider"
    cfg["active_provider"] = "claude-code"
    cfg["active_provider_models"] = [{"provider_id": "claude-code", "model": MODEL}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.pop("model_tiers", None)
    cfg.setdefault("context", {})["on_overflow"] = "compact"
    # 尾巴预算默认 16384 tok 会把四轮小会话整个留住(报「没有可压缩的内容」);
    # 压到最小,只有 MIN_TAIL_TURNS=2 轮留在尾巴,文件轮才进折叠区。
    cfg["context"]["compact_tail_tokens"] = 1
    cfg.setdefault("cache", {})["request_log"] = True
    cfg.setdefault("display", {})["show_token_usage"] = False
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    real_dev = REAL_CONFIG.parent / "dev-prompt.md"
    if real_dev.exists():
        shutil.copy(real_dev, HOME / "config" / "dev-prompt.md")
    real_cache = Path.home() / ".miyu" / "cache" / "models_cache.json"
    if real_cache.exists():
        (HOME / "cache").mkdir(parents=True, exist_ok=True)
        shutil.copy(real_cache, HOME / "cache" / "models_cache.json")


def env():
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["XDG_RUNTIME_DIR"] = str(RUN)
    for key in ("MIYU_DIRECT", "MIYU_SESSION", "MIYU_TURN_MODE",
                "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"):
        e.pop(key, None)
    e["LANG"] = "zh_CN.UTF-8"
    return e


def cli(args, timeout=600):
    proc = subprocess.run([str(MIYU), *args], env=env(), cwd=WORK, capture_output=True,
                          text=True, timeout=timeout)
    print(f"$ miyu {' '.join(args)[:100]}\n  code={proc.returncode} out={proc.stdout.strip()[:200]!r}")
    if proc.stderr.strip():
        print(f"  err={proc.stderr.strip()[-300:]!r}")
    return proc


def db_rows(sql):
    # 活库三件一起拷副本再查,不碰正在写的库。
    src = HOME / "state" / "conversation.db"
    snap = BASE / "snap"
    if snap.exists():
        shutil.rmtree(snap)
    snap.mkdir()
    for suffix in ("", "-wal", "-shm"):
        p = Path(str(src) + suffix)
        if p.exists():
            shutil.copy(p, snap / p.name)
    conn = sqlite3.connect(snap / "conversation.db")
    return conn.execute(sql).fetchall()


def main():
    build_home()
    session = "relayprobe"
    # 折叠区得超过 MIN_FOLD_TOKENS=400:文件给大一点,Read 结果撑起折叠区体积。
    filler = "".join(f"line {i}: the quick brown fox jumps over the lazy dog\n" for i in range(120))
    (WORK / "notes.txt").write_text("alpha\nbeta\n" + filler, encoding="utf-8")
    ok = True
    try:
        cli(["ask", "--output-format", "json", "--session", session, "--create", "--mode", "dev",
             "Use your Read tool to read notes.txt in the current directory, then use your Edit tool "
             "to change the line 'beta' to 'gamma'. Reply with one word when done."])
        cli(["ask", "--output-format", "json", "--session", session,
             "Use your Write tool to create hello.txt containing 'hi'. Reply with one word."])
        cli(["ask", "--output-format", "json", "--session", session, "Reply with exactly: ok"])
        cli(["ask", "--output-format", "json", "--session", session, "Reply with exactly: fine"])
        print("turn footprints:")
        for seq, fp in db_rows("select seq, tool_footprint from turns order by seq"):
            print(f"  seq={seq} footprint={fp}")
        cli(["compact", "--session", session], timeout=900)
        time.sleep(1)
        rows = db_rows("select seq, assistant_content, compact_extras from turns where is_summary=1")
        if not rows:
            print("✗ 没有摘要行:压缩没发生")
            ok = False
        for seq, summary, extras in rows:
            has_read = "<read-files>" in summary
            has_mod = "<modified-files>" in summary
            restored = [f["path"] for f in json.loads(extras or "{}").get("restored", [])]
            print(f"  summary seq={seq} read-files={has_read} modified-files={has_mod} restored={restored}")
            ok = ok and has_read and has_mod and any(p.endswith("notes.txt") for p in restored)
    finally:
        cli(["daemon", "stop"], timeout=60)
    print("PASS" if ok else "FAIL", "| probe dir:", BASE)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
