#!/usr/bin/env python3
"""事实整理效果 A/B:把真实记忆库拷进沙箱,让某个二进制的整理器跑几批,看它产出什么。

真实库里 400 多条短期日记是现成原料。沙箱 daemon 启动时会唤醒整理器
(每次唤醒最多 4 批 × 14 条),跑完后把新产出的事实倒出来,量:条数、平均长度、
超 120 字占比,再把内容列出来供肉眼判断「是通用知识还是真记忆」。

会花真实模型的额度(整理器走 memory_organizer 档位的池)。

    BIN=~/.local/bin/miyu TAG=before python3 testkit/memory-quality/replay.py
    BIN=target/debug/miyu   TAG=after  python3 testkit/memory-quality/replay.py

环境变量:SRC_HOME(默认 ~/.miyu)、PERSONA(默认 default)、DIARIES(取最近多少条
短期日记重置为未整理,默认 42 = 3 批)、WAIT(等整理秒数,默认 240)。
"""

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("BIN", REPO / "target" / "debug" / "miyu")).expanduser()
SRC_HOME = Path(os.environ.get("SRC_HOME", "~/.miyu")).expanduser()
PERSONA = os.environ.get("PERSONA", "default")
TAG = os.environ.get("TAG", "run")
DIARIES = int(os.environ.get("DIARIES", "42"))
WAIT = int(os.environ.get("WAIT", "240"))
PORT = int(os.environ.get("PORT", "18461"))
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-memory-quality")).expanduser() / TAG
HOME = OUT / "home"
RUNTIME = OUT / "run"


ORGANIZER_POOL = json.loads(os.environ.get(
    "ORGANIZER_POOL", '[{"provider_id": "ririxin", "model": "deepseek-v4-flash"}]'))


def write_sandbox_config(src, dst):
    """只带供应商与记忆配置进沙箱:平台(QQ)、语音、MCP、通知一律不带,免得
    沙箱 daemon 连上真 QQ 或拉起 MCP。整理器固定走 ORGANIZER_POOL 指定的池,
    A/B 两边模型一致,量的才是提示词的差别;默认不走 claude-code,不烧订阅额度。"""
    config = json.loads(src.read_text())
    for key in ("platforms", "voice", "mcp", "notifications", "system_prompt_file", "skills"):
        config.pop(key, None)
    tiers = config.setdefault("model_tiers", {})
    tiers["standard"] = ORGANIZER_POOL
    tiers.setdefault("roles", {})["memory_organizer"] = "standard"
    config.setdefault("tools", {})["enabled"] = False
    dst.write_text(json.dumps(config, ensure_ascii=False, indent=2))


def copy_db(src, dst):
    dst.parent.mkdir(parents=True, exist_ok=True)
    for suffix in ("", "-wal", "-shm"):
        if (src.parent / (src.name + suffix)).exists():
            shutil.copy(src.parent / (src.name + suffix), dst.parent / (dst.name + suffix))


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    (HOME / "config").mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_sandbox_config(SRC_HOME / "config" / "config.jsonc", HOME / "config" / "config.jsonc")
    memory_db = SRC_HOME / "data" / "personas" / PERSONA / "memory" / "memory.db"
    target = HOME / "data" / "personas" / PERSONA / "memory" / "memory.db"
    copy_db(memory_db, target)
    con = sqlite3.connect(target)
    con.execute("PRAGMA journal_mode=DELETE")
    before_ids = {row[0] for row in con.execute("SELECT id FROM facts")}
    before_long = {row[0] for row in con.execute("SELECT id FROM episodes WHERE retention='long_term'")}
    # 把最近 N 条短期日记重置为未整理,整理器就会重新啃它们。
    ids = [row[0] for row in con.execute(
        "SELECT id FROM episodes WHERE retention='short_term' AND status='active' ORDER BY id DESC LIMIT ?", (DIARIES,))]
    con.execute(f"UPDATE episodes SET consolidated_at=NULL, promotion_pending=0 WHERE id IN ({','.join('?'*len(ids))})", ids)
    con.commit()
    con.close()
    print(f"· 重置 {len(ids)} 条日记为未整理;库里已有事实 {len(before_ids)} 条", flush=True)

    env = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME), MIYU_LOG_REQUESTS="1")
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=env, cwd=str(HOME),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    try:
        deadline = time.time() + WAIT
        while time.time() < deadline:
            time.sleep(10)
            con = sqlite3.connect(f"file:{target}?mode=ro", uri=True)
            pending = con.execute("SELECT count(*) FROM episodes WHERE retention='short_term' AND consolidated_at IS NULL").fetchone()[0]
            con.close()
            print(f"  待整理 {pending}", flush=True)
            if pending == 0:
                break
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()

    con = sqlite3.connect(f"file:{target}?mode=ro", uri=True)
    new_facts = [row for row in con.execute(
        "SELECT id, visibility, truth_status, importance, content FROM facts ORDER BY id") if row[0] not in before_ids]
    new_long = [row for row in con.execute(
        "SELECT id, content FROM episodes WHERE retention='long_term' ORDER BY id") if row[0] not in before_long]
    revisions = con.execute("SELECT count(*) FROM memory_revisions").fetchone()[0]
    con.close()
    lengths = [len(row[4]) for row in new_facts]
    summary = {
        "tag": TAG, "bin": str(BIN), "diaries": len(ids),
        "new_facts": len(new_facts), "new_long_diaries": len(new_long),
        "avg_len": round(sum(lengths) / len(lengths), 1) if lengths else 0,
        "over_120": sum(1 for n in lengths if n > 120),
        "revisions_total": revisions,
    }
    print(json.dumps(summary, ensure_ascii=False), flush=True)
    (OUT / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=2))
    with (OUT / "facts.txt").open("w") as f:
        for row in new_facts:
            f.write(f"{row[0]} | {row[1]} | {row[2]} | {row[3]} | {row[4]}\n")
        f.write("\n== long diaries ==\n")
        for row in new_long:
            f.write(f"{row[0]} | {row[1]}\n")
    print(f"· 明细 {OUT / 'facts.txt'}", flush=True)


if __name__ == "__main__":
    main()
