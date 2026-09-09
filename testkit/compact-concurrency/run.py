#!/usr/bin/env python3
"""压缩期间其他会话还能不能用（09-09 实况事故的回归测具）。

事故：一次 `miyu compact` 把**所有**会话拖死四分半。根因是
`ActorCommand::Compact` 在 actor 主循环里同步 await 整个压缩，而回合是
`spawn_local` 出去的——压缩几分钟，actor 就几分钟收不到任何命令，所有会话的
StartTurn 全排在 mpsc 队列里。

测法：桩 LLM 对摘要请求故意睡 25 秒。在会话 A 上发起压缩，2 秒后去会话 B
发一句话，量 B 的往返耗时。

    修好了：B 立刻回（几秒），压缩在后台继续
    没修好：B 一直等到 A 的压缩跑完（≈25 秒）

退回修复前这条必红——这正是它要守的东西。

用法（先 cargo build）：python3 testkit/compact-concurrency/run.py
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BASE = Path(__file__).resolve().parent
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
OUT = BASE / "out"
# unix socket 有 SUN_LEN(108B)上限，worktree 路径太深，运行目录放短路径。
WORK = Path.home() / ".cache" / "miyu-compact-conc"
HOME = WORK / "home"
PORT = 18762
STUB_PORT = 18761
COMPACT_SECS = 25
# 压缩至少要慢这么多，B 才有意义地「插队」成功。
B_MUST_ANSWER_WITHIN = 12.0

spec = importlib.util.spec_from_file_location(
    "persona_ab", REPO / "testkit" / "persona-ab" / "run.py"
)
persona_ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(persona_ab)

results = []


def check(name, ok, detail=""):
    results.append({"name": name, "ok": bool(ok), "detail": str(detail)[:300]})
    print(f"{'✅' if ok else '❌'} {name}  {detail}", flush=True)


def env():
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["XDG_RUNTIME_DIR"] = str(WORK / "run")
    e["LANG"] = "zh_CN.UTF-8"
    for key in ("MIYU_DIRECT", "MIYU_SESSION", "MIYU_TURN_MODE", "XDG_CACHE_HOME",
                "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME",
                "MIYU_COMPACT_PROMPT_FILE", "MIYU_COMPACT_ANALYSIS"):
        e.pop(key, None)
    return e


def build_home():
    if WORK.exists():
        shutil.rmtree(WORK)
    (WORK / "run").mkdir(parents=True)
    (HOME / "config").mkdir(parents=True)
    OUT.mkdir(parents=True, exist_ok=True)
    cfg = persona_ab.load_real_config()
    for key in ("platforms", "web", "voice", "alarm"):
        cfg.pop(key, None)
    cfg["providers"] = [{
        "enabled": True, "id": "stub", "display_name": "Stub",
        "base_url": f"http://127.0.0.1:{STUB_PORT}/v1", "protocol": "openai-chat",
        "api_key": "stub-key", "models": ["stub-a"],
        "model_context_window": {"stub-a": 32000},
    }]
    cfg["active_provider"] = "stub"
    cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-a"}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.setdefault("prompt", {})["active_persona"] = ""
    cfg["prompt"]["persona_reminder"] = False
    cfg.setdefault("tools", {})["enabled"] = False
    cfg.setdefault("memory", {})["enabled"] = False
    cfg.setdefault("skills", {})["enabled"] = False
    context = cfg.setdefault("context", {})
    # 尾巴收窄，几轮小对话就够触发折叠；播种期间不许自动压缩。
    context["compact_tail_tokens"] = 200
    context["trim_at_ratio"] = 0.95
    context["compact_force_ratio"] = 0.99
    context["compact_restore_files"] = 0
    context["compact_transcript_export"] = False
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def find_socket():
    for path in (WORK / "run").rglob("*.sock"):
        return path
    return None


def cli(args, stdin=None, timeout=180):
    proc = subprocess.run([str(MIYU), *args], env=env(), input=stdin,
                          capture_output=True, text=True, timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr


def ask(session, text, create=False, timeout=180):
    args = ["ask", "--output-format", "json", "--session", session]
    if create:
        args.append("--create")
    args.append("--stdin")
    return cli(args, stdin=text, timeout=timeout)


def main():
    if not MIYU.exists():
        raise SystemExit(f"missing binary {MIYU}; run cargo build")
    build_home()
    stub = subprocess.Popen(
        [sys.executable, str(BASE / "stub.py")],
        env=dict(os.environ, STUB_PORT=str(STUB_PORT), STUB_COMPACT_SECS=str(COMPACT_SECS)),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    daemon = subprocess.Popen(
        [str(MIYU), "daemon", "--port", str(PORT)], env=env(),
        stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT,
    )
    try:
        for _ in range(80):
            if find_socket():
                break
            time.sleep(0.5)
        if not find_socket():
            raise SystemExit("daemon socket never appeared")
        time.sleep(1.5)

        # 会话 A：够折叠的历史（尾巴预算 200 tok，5 轮就够）。
        filler = "compact concurrency fixture line with several ordinary words\n" * 60
        ask("alpha", f"A1 {filler}", create=True)
        for i in range(2, 6):
            ask("alpha", f"A{i} {filler}")
        # 会话 B：存在即可。
        ask("beta", "B0 hello", create=True)

        compact_result = {}

        def run_compact():
            started = time.time()
            code, out, err = cli(["compact", "--session", "alpha"], timeout=180)
            compact_result.update(code=code, out=out, err=err, secs=time.time() - started)

        thread = threading.Thread(target=run_compact)
        thread.start()
        # 让压缩真的进到摘要请求里（桩在那儿睡 25 秒）。
        time.sleep(3.0)

        started = time.time()
        code, out, err = ask("beta", "B1 还在吗", timeout=120)
        beta_secs = time.time() - started
        check(
            "压缩进行中,另一个会话仍能正常回合",
            code == 0 and beta_secs < B_MUST_ANSWER_WITHIN,
            f"beta 往返 {beta_secs:.1f}s(上限 {B_MUST_ANSWER_WITHIN}s,"
            f"压缩桩睡 {COMPACT_SECS}s) code={code} err={err.strip()[:120]}",
        )
        check(
            "beta 是在压缩结束前答完的",
            beta_secs < COMPACT_SECS - 5,
            f"{beta_secs:.1f}s < {COMPACT_SECS - 5}s",
        )

        thread.join(timeout=180)
        check(
            "压缩自己也正常完成",
            compact_result.get("code") == 0 and "已压缩" in compact_result.get("out", ""),
            f"code={compact_result.get('code')} secs={compact_result.get('secs', 0):.1f} "
            f"out={compact_result.get('out', '').strip()[:80]}",
        )
        check(
            "压缩耗时确实盖过了桩的睡眠",
            compact_result.get("secs", 0) >= COMPACT_SECS,
            f"{compact_result.get('secs', 0):.1f}s >= {COMPACT_SECS}s",
        )
    finally:
        subprocess.run([str(MIYU), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()
        stub.terminate()
        OUT.mkdir(parents=True, exist_ok=True)
        (OUT / "verdict.json").write_text(
            json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8"
        )

    passed = sum(1 for r in results if r["ok"])
    print(f"\n{passed}/{len(results)} passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
