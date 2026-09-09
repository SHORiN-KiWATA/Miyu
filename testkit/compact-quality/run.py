#!/usr/bin/env python3
"""压缩质量 A/B 实测（compact v3，2026-09-09）：真模型、隔离 home、逐题记分。

一张表回答三个问题：新提示词提升多少召回；压后回灌+折叠转录再提升多少；
分析段值不值那点输出成本。外加一张「本地估算 vs 供应商锚点」的误差表。

流程
    1. 播种：在 fixture 副本里跑 25 条脚本化提示（读文件、改文件、被纠正一次、
       声明若干规则），事实全落在前 20 轮，最后几轮是无关闲聊——保证 2 轮逐字
       尾巴里没有答案。播完整份 home 拷成 home.seeded。
    2. 变体：每个变体从 home.seeded 恢复、改配置/环境、`miyu compact`，再把压
       完的 home 拷成 home.compacted-<变体>。
    3. 答题：每题都从 home.compacted-<变体> 恢复后单独问，答完丢弃——20 道题
       互不污染，且每题都是「压完第一句话」。关键字命中即得分。

    变体      提示词   分析段   回灌   转录
    V0 基线   v2       无       关     关
    V1        v3       自动开   关     关
    V2        v3       开       开     开
    V3        v3       强制关   开     开

用法（先 cargo build）：
    python3 testkit/compact-quality/run.py --rescore                # 只重打分，不跑模型
    python3 testkit/compact-quality/run.py --provider deepseek --model deepseek-v4-flash
    python3 testkit/compact-quality/run.py --variants V0,V2      # 只跑两个
    python3 testkit/compact-quality/run.py --quiz-limit 5        # 冒烟

绝不触碰线上 8300 daemon：独立 MIYU_HOME、独立端口、独立 XDG_RUNTIME_DIR。
产物：testkit/compact-quality/out/（compact-quality.md、verdict.json、摘要文本）。
"""
import argparse
import importlib.util
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BASE = Path(__file__).resolve().parent
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
OUT = BASE / "out"
# unix socket 有 SUN_LEN(108B)上限，worktree 路径太深，运行目录放短路径。
WORK = Path.home() / ".cache" / "miyu-compact-quality"
HOME = WORK / "home"
SEEDED = WORK / "home.seeded"
PROJECT = WORK / "project"
PORT = 18613
SESSION = "quiz"

spec = importlib.util.spec_from_file_location(
    "persona_ab", REPO / "testkit" / "persona-ab" / "run.py"
)
persona_ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(persona_ab)

VARIANTS = {
    # 名字 → (提示词文件 or None, 分析段 "auto"/"off", 回灌开关, 转录开关)
    "V0": ("prompt-v2.md", "auto", False, False),
    "V1": (None, "auto", False, False),
    "V2": (None, "auto", True, True),
    "V3": (None, "off", True, True),
}

results = []
daemon = None


def log(text):
    print(text, flush=True)


# ------------------------------------------------------------------ 环境


def env(extra=None):
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["XDG_RUNTIME_DIR"] = str(WORK / "run")
    e["MIYU_LOG"] = "info"
    e["LANG"] = "zh_CN.UTF-8"
    for key in ("MIYU_DIRECT", "MIYU_SESSION", "MIYU_TURN_MODE", "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "MIYU_COMPACT_PROMPT_FILE", "MIYU_COMPACT_ANALYSIS"):
        e.pop(key, None)
    if extra:
        e.update(extra)
    return e


def write_config(provider_id, model, restore, transcript):
    cfg = persona_ab.load_real_config()
    for key in ("platforms", "web", "voice", "alarm"):
        cfg.pop(key, None)
    providers = [p for p in cfg.get("providers", []) if p.get("id") == provider_id]
    if not providers:
        raise SystemExit(
            f"供应商 {provider_id} 不在 ~/.miyu/config/config.jsonc 里；"
            f"可选：{[p.get('id') for p in cfg.get('providers', [])]}"
        )
    providers[0]["enabled"] = True
    cfg["providers"] = providers
    cfg["active_provider"] = provider_id
    cfg["active_provider_models"] = [{"provider_id": provider_id, "model": model}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.setdefault("prompt", {})["active_persona"] = ""
    cfg["prompt"]["persona_reminder"] = False
    cfg.setdefault("tools", {})["enabled"] = True
    cfg.setdefault("memory", {})["enabled"] = False
    cfg.setdefault("skills", {})["enabled"] = False
    context = cfg.setdefault("context", {})
    # 播种期间不许自动压缩：什么时候压、压几次由测具说了算。
    context["trim_at_ratio"] = 0.95
    context["compact_force_ratio"] = 0.99
    context["compact_restore_files"] = 5 if restore else 0
    context["compact_transcript_export"] = bool(transcript)
    # 逐字尾巴收到 1200 tok：默认的 min(16384, 窗口/4) 在这个规模的会话上
    # 一轮都折不掉（切点=0，压缩正确地什么都不做）。收窄之后最后两三轮闲聊
    # 留在尾巴里逐字保留，前面 20 轮事实全部进折叠区——正是要测的形状。
    context["compact_tail_tokens"] = 1200
    cfg.setdefault("display", {})["show_token_usage"] = False
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def find_socket():
    for path in (WORK / "run").rglob("*.sock"):
        return path
    return None


def start_daemon():
    global daemon
    OUT.mkdir(parents=True, exist_ok=True)
    daemon = subprocess.Popen(
        [str(MIYU), "daemon", "--port", str(PORT)],
        env=env(),
        cwd=str(PROJECT),
        stdout=(OUT / "daemon.log").open("a"),
        stderr=subprocess.STDOUT,
    )
    for _ in range(80):
        if find_socket():
            time.sleep(1.5)
            return
        time.sleep(0.5)
    raise SystemExit("daemon socket never appeared")


def stop_daemon():
    global daemon
    if daemon is None:
        return
    subprocess.run([str(MIYU), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
    try:
        daemon.wait(timeout=15)
    except subprocess.TimeoutExpired:
        daemon.kill()
    daemon = None


def cli(args, stdin=None, timeout=300, extra_env=None):
    proc = subprocess.run(
        [str(MIYU), *args],
        env=env(extra_env),
        cwd=str(PROJECT),
        input=stdin,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return proc.returncode, proc.stdout, proc.stderr


def ask(prompt, create=False, timeout=300, extra_env=None):
    args = ["ask", "--output-format", "json", "--session", SESSION]
    if create:
        args.append("--create")
    # 正文只走 stdin：提示里有引号和中文，位置参数容易被 clap 的
    # trailing_var_arg 误读，而空 message + stdin 就是纯正文。
    args.append("--stdin")
    code, out, err = cli(args, stdin=prompt, timeout=timeout, extra_env=extra_env)
    text = ""
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "done":
            text = event.get("text", "")
    return code, text, err


# ------------------------------------------------------------------ DB 取数


def conv_db():
    for path in HOME.rglob("conversation.db"):
        return path
    raise SystemExit("conversation.db not found")


def query(sql, params=()):
    path = conv_db()
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return con.execute(sql, params).fetchall()
    finally:
        con.close()


def session_id():
    rows = query("SELECT session_id FROM sessions WHERE name = ?", (SESSION,))
    if not rows:
        raise SystemExit(f"session {SESSION} not found")
    return rows[0][0]


def summary_row():
    """摘要行：正文、extras、以及摘要请求自己的用量（含缓存命中）。"""
    rows = query(
        "SELECT assistant_content, compact_extras, token_total, token_prompt, token_cache_read "
        "FROM turns WHERE session_id = ? AND hidden = 0 AND is_summary = 1 "
        "ORDER BY seq DESC LIMIT 1",
        (session_id(),),
    )
    return rows[0] if rows else ("", None, 0, 0, 0)


def last_turn_tool_flow():
    rows = query(
        "SELECT tool_flow FROM turns WHERE session_id = ? AND hidden = 0 AND is_summary = 0 "
        "ORDER BY seq DESC LIMIT 1",
        (session_id(),),
    )
    if not rows or not rows[0][0]:
        return []
    try:
        return json.loads(rows[0][0])
    except json.JSONDecodeError:
        return []


def read_calls_in_last_turn():
    total = 0
    for round_ in last_turn_tool_flow():
        for call in round_.get("calls", []):
            if call.get("name") in ("read", "read_file"):
                total += 1
    return total


def context_tokens():
    code, out, _ = cli(["session", "show", SESSION, "--json"], timeout=120)
    if code != 0:
        return 0
    try:
        return json.loads(out).get("context_tokens", 0)
    except json.JSONDecodeError:
        return 0


METER = re.compile(r"context meter.*?estimate[=:]\s*(\d+).*?anchor[=:]\s*(\d+)")


def meter_samples():
    """从 daemon 日志里捞 `context meter estimate=.. anchor=..`。"""
    samples = []
    log_dir = HOME / "cache" / "logs"
    candidates = sorted(log_dir.glob("miyu*.log")) if log_dir.is_dir() else []
    for path in candidates + [OUT / "daemon.log"]:
        if not path.exists():
            continue
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            match = METER.search(line)
            if match:
                samples.append((int(match.group(1)), int(match.group(2))))
    return samples


# ------------------------------------------------------------------ 播种


def seed(args):
    if WORK.exists():
        shutil.rmtree(WORK)
    (WORK / "run").mkdir(parents=True)
    shutil.copytree(BASE / "fixture", PROJECT)
    HOME.mkdir(parents=True)
    write_config(args.provider, args.model, restore=False, transcript=False)
    real_cache = Path.home() / ".miyu" / "cache" / "models_cache.json"
    if real_cache.exists():
        (HOME / "cache").mkdir(parents=True, exist_ok=True)
        shutil.copy(real_cache, HOME / "cache" / "models_cache.json")

    start_daemon()
    prompts = json.loads((BASE / "seed.json").read_text(encoding="utf-8"))
    if args.seed_limit:
        prompts = prompts[: args.seed_limit]
    for index, prompt in enumerate(prompts, start=1):
        code, text, err = ask(prompt, create=(index == 1), timeout=args.timeout)
        log(f"  seed {index:>2}/{len(prompts)}  code={code}  {text.strip()[:70]!r}")
        if code != 0:
            log(f"    stderr: {err.strip()[:200]}")
    before = context_tokens()
    stop_daemon()
    shutil.copytree(HOME, SEEDED)
    log(f"播种完成：{len(prompts)} 轮，压前上下文 {before} tok")
    return before


# ------------------------------------------------------------------ 变体


def restore_home(source):
    if HOME.exists():
        shutil.rmtree(HOME)
    shutil.copytree(source, HOME)


def run_variant(name, args, before_tokens):
    prompt_file, analysis, restore, transcript = VARIANTS[name]
    log(f"\n=== 变体 {name} ===")
    restore_home(SEEDED)
    write_config(args.provider, args.model, restore=restore, transcript=transcript)
    extra = {}
    if prompt_file:
        extra["MIYU_COMPACT_PROMPT_FILE"] = str(BASE / prompt_file)
    if analysis == "off":
        extra["MIYU_COMPACT_ANALYSIS"] = "0"

    start_daemon()
    started = time.time()
    code, out, err = cli(["compact", "--session", SESSION], timeout=args.timeout, extra_env=extra)
    elapsed = time.time() - started
    if code != 0:
        log(f"  compact 失败 code={code}: {err.strip()[:200]}")
    after = context_tokens()
    summary, extras_json, total_tokens, prompt_tokens, cache_read = summary_row()
    stop_daemon()

    compacted = WORK / f"home.compacted-{name}"
    if compacted.exists():
        shutil.rmtree(compacted)
    shutil.copytree(HOME, compacted)
    (OUT / f"summary-{name}.md").write_text(summary, encoding="utf-8")
    if extras_json:
        (OUT / f"extras-{name}.json").write_text(extras_json, encoding="utf-8")

    quiz = json.loads((BASE / "quiz.json").read_text(encoding="utf-8"))
    if args.quiz_limit:
        quiz = quiz[: args.quiz_limit]
    score = 0
    reads = 0
    answers = []
    for item in quiz:
        restore_home(compacted)
        write_config(args.provider, args.model, restore=restore, transcript=transcript)
        start_daemon()
        _, text, _ = ask(item["q"], timeout=args.timeout, extra_env=extra)
        reads += read_calls_in_last_turn()
        stop_daemon()
        hit = any(key.lower() in text.lower() for key in item["accept"])
        score += 1 if hit else 0
        answers.append({"id": item["id"], "group": item["group"], "hit": hit, "answer": text.strip()[:400]})
        log(f"  {'✅' if hit else '❌'} {item['id']:<16} {text.strip()[:60]!r}")

    record = {
        "variant": name,
        "prompt": prompt_file or "v3",
        "analysis": analysis,
        "restore": restore,
        "transcript": transcript,
        "score": score,
        "total": len(quiz),
        "quiz_reads": reads,
        "before_tokens": before_tokens,
        "after_tokens": after,
        "summary_chars": len(summary),
        "compact_seconds": round(elapsed, 1),
        "summary_total_tokens": total_tokens or 0,
        "summary_prompt_tokens": prompt_tokens or 0,
        "summary_cache_read": cache_read or 0,
        "summary_output_tokens": max((total_tokens or 0) - (prompt_tokens or 0), 0),
        "answers": answers,
    }
    results.append(record)
    log(f"  {name}: {score}/{len(quiz)}  reads={reads}  {before_tokens}→{after} tok  {elapsed:.1f}s")
    return record


# ------------------------------------------------------------------ 报表


def percent(part, whole):
    return f"{100 * part / whole:.1f}%" if whole else "—"


def write_report(samples):
    lines = ["# 压缩质量 A/B 实测", ""]
    lines.append("| 变体 | 提示词 | 分析段 | 回灌 | 召回 | 答题 read 次数 | 压前 tok | 压后 tok | 摘要输出 tok | 摘要字符 | 压缩耗时 s | 摘要请求缓存命中 |")
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for record in results:
        cache = "—"
        if record["summary_prompt_tokens"]:
            cache = percent(record["summary_cache_read"], record["summary_prompt_tokens"])
        lines.append(
            f"| {record['variant']} | {record['prompt']} | {record['analysis']} | "
            f"{'开' if record['restore'] else '关'} | {record['score']}/{record['total']} | "
            f"{record['quiz_reads']} | {record['before_tokens']} | {record['after_tokens']} | "
            f"{record['summary_output_tokens']} | {record['summary_chars']} | "
            f"{record['compact_seconds']} | {cache} |"
        )

    lines += ["", "## 分组召回", "", "| 变体 | 标准事实 | 文件内容 | 决策与纠正 | 当前工作 | 错误 |", "|---|---|---|---|---|---|"]
    groups = ["fact", "file", "decision", "current", "error"]
    for record in results:
        cells = []
        for group in groups:
            items = [a for a in record["answers"] if a["group"] == group]
            cells.append(f"{sum(1 for a in items if a['hit'])}/{len(items)}" if items else "—")
        lines.append(f"| {record['variant']} | " + " | ".join(cells) + " |")

    lines += ["", "## 上下文量尺：本地估算 vs 供应商锚点", ""]
    if samples:
        lines += ["| # | estimate | anchor | 相对误差 |", "|---|---|---|---|"]
        errors = []
        for index, (estimate, anchor) in enumerate(samples, start=1):
            error = abs(estimate - anchor) / anchor if anchor else 0
            errors.append(error)
            lines.append(f"| {index} | {estimate} | {anchor} | {error * 100:.1f}% |")
        lines.append("")
        lines.append(
            f"均值误差 {sum(errors) / len(errors) * 100:.1f}%，最大 {max(errors) * 100:.1f}%，"
            f"样本 {len(errors)} 条。"
        )
    else:
        lines.append("（没抓到 `context meter` 日志行；确认 MIYU_LOG=info 且跑过至少一轮完整回合。）")

    (OUT / "compact-quality.md").write_text("\n".join(lines) + "\n", encoding="utf-8")
    log("\n" + "\n".join(lines))


# ------------------------------------------------------------------ main


def rescore():
    """按当前 quiz.json 的关键字，对已存的答案重新记分并重出报告。

    关键字表写错了（比如漏了她用第二人称复述的说法）不该让整轮真模型白跑：
    答案原文已经在 verdict.json 里，重打分是纯离线的，而且四个变体一起重打，
    口径始终一致。
    """
    stored = json.loads((OUT / "verdict.json").read_text(encoding="utf-8"))
    quiz = {item["id"]: item for item in json.loads((BASE / "quiz.json").read_text(encoding="utf-8"))}
    for record in stored:
        score = 0
        for answer in record["answers"]:
            item = quiz.get(answer["id"])
            if item is None:
                continue
            answer["hit"] = any(key.lower() in answer["answer"].lower() for key in item["accept"])
            score += 1 if answer["hit"] else 0
        record["score"] = score
        results.append(record)
        log(f"{record['variant']}: {score}/{record['total']}")
    (OUT / "verdict.json").write_text(
        json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    write_report(meter_samples())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--provider", default="deepseek")
    parser.add_argument("--model", default="deepseek-v4-flash")
    parser.add_argument("--variants", default="V0,V1,V2,V3")
    parser.add_argument("--timeout", type=int, default=420)
    parser.add_argument("--seed-limit", type=int, default=0, help="只跑前 N 条播种提示（冒烟用）")
    parser.add_argument("--quiz-limit", type=int, default=0, help="只答前 N 道题（冒烟用）")
    parser.add_argument("--skip-seed", action="store_true", help="复用上次的 home.seeded")
    parser.add_argument("--rescore", action="store_true",
                        help="不跑模型，按当前 quiz.json 对 out/verdict.json 里的答案重新记分")
    args = parser.parse_args()

    if args.rescore:
        rescore()
        return

    if not MIYU.exists():
        raise SystemExit(f"missing binary {MIYU}; run cargo build")
    OUT.mkdir(parents=True, exist_ok=True)

    try:
        if args.skip_seed and SEEDED.exists():
            restore_home(SEEDED)
            start_daemon()
            before = context_tokens()
            stop_daemon()
            log(f"复用已播种的 home，压前上下文 {before} tok")
        else:
            before = seed(args)

        for name in args.variants.split(","):
            name = name.strip()
            if name:
                run_variant(name, args, before)
    finally:
        stop_daemon()
        (OUT / "verdict.json").write_text(
            json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
        )
        if results:
            write_report(meter_samples())

    ok = all(record["score"] > 0 for record in results)
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
