#!/usr/bin/env python3
"""把 measure.py 产出的 measure-<label>.json 汇总成相位表(Markdown)。
用法: summarize.py measure-a.json [measure-b.json ...]"""
import json, sys, statistics as st

PHASES = ["quiet", "speech-nowake", "wake-stt", "stt-warm", "quiet-after"]
for path in sys.argv[1:]:
    data = json.load(open(path))
    samples, events = data["samples"], data["events"]
    print(f"\n### {path}")
    print("| 相位 | CPU 均值 | CPU 峰值 | RSS 起→止 | RSS 高水位 | 线程 |")
    print("|---|---|---|---|---|---|")
    for phase in PHASES:
        rows = [s for s in samples if s["phase"] == phase]
        if not rows:
            continue
        print(f"| {phase} | {st.mean(r['cpu'] for r in rows):.1f}% | {max(r['cpu'] for r in rows):.0f}% | "
              f"{rows[0]['rss']}→{rows[-1]['rss']}MB | {rows[-1]['hwm']}MB | {rows[-1]['thr']} |")
    timings = {}
    for _, line in events:
        if "[timing]" not in line:
            continue
        parts = line.split("[timing]")[1].split()
        stage = parts[0]
        audio = float(parts[1].split("=")[1].rstrip("s"))
        ms = int(parts[2].split("=")[1].rstrip("ms"))
        timings.setdefault(stage, []).append((audio, ms))
    print("\n| 阶段 | 次数 | 耗时中位 | 耗时最大 | 每秒音频耗时(中位) |")
    print("|---|---|---|---|---|")
    for stage, rows in timings.items():
        ms = [r[1] for r in rows]
        per_sec = [r[1] / r[0] for r in rows if r[0] > 0]
        print(f"| {stage} | {len(rows)} | {st.median(ms):.0f}ms | {max(ms)}ms | {st.median(per_sec):.0f}ms/s |")
