#!/usr/bin/env python3
"""CLI 体系黑盒实测(09-07 cli-overhaul):隔离 home + 独立端口 daemon + 桩 LLM。

桩 LLM(stub_llm.py)把「它看到的请求」当正文回来,所以每个覆盖参数都能从
回复 JSON 里直接读出有没有生效。覆盖场景:

    json / stream-json 输出形状与退出码
    --model / --system-prompt / --append-system-prompt / --tools / --no-tools /
    --context-window / --no-memory(memory.db 的 episodes 行数不变)
    --session X --create / 历史延续 / session list|show|clear|rename|delete / 会话不存在退出码 3
    --stdin 长输入不截断
    --timeout → 退出码 124;模型返回 500 → 退出码 1
    miyu stdio:ready / 并发两回合事件归属 / question→answer 往返 / cancel / session op / ping / EOF 退出

用法:先 `cargo build`,再 `python3 testkit/cli/run.py`。绝不触碰线上 8300 daemon。
产物:testkit/cli/out/(daemon.log、stub.jsonl、verdict.json)。
"""
import importlib.util
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
from queue import Empty, Queue
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
BASE = Path(__file__).resolve().parent
OUT = BASE / "out"
HOME = BASE / "home"
# unix socket 有 SUN_LEN(108B)上限,worktree 路径太深,运行目录放短路径。
RUN = Path.home() / ".cache" / "miyu-cli-tk-run"
PORT = 18395
STUB_PORT = 18494

spec = importlib.util.spec_from_file_location("persona_ab", REPO / "testkit" / "persona-ab" / "run.py")
persona_ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(persona_ab)


def build_home():
    for path in (HOME, RUN, OUT):
        if path.exists():
            shutil.rmtree(path)
    (HOME / "config").mkdir(parents=True)
    RUN.mkdir(parents=True)
    OUT.mkdir(parents=True)
    cfg = persona_ab.load_real_config()
    for key in ("platforms", "web", "voice", "alarm"):
        cfg.pop(key, None)
    cfg["providers"] = [{
        "enabled": True, "id": "stub", "display_name": "Stub",
        "base_url": f"http://127.0.0.1:{STUB_PORT}/v1", "protocol": "openai-chat",
        "api_key": "stub-key", "models": ["stub-a", "stub-b"],
    }]
    cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-a"}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.setdefault("prompt", {})["active_persona"] = ""
    memory = cfg.setdefault("memory", {})
    memory["enabled"] = True
    memory["association_enabled"] = False
    cfg.setdefault("cache", {})["request_log"] = False
    cfg.setdefault("display", {})["show_token_usage"] = False
    (HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    real_cache = Path.home() / ".miyu" / "cache" / "models_cache.json"
    if real_cache.exists():
        (HOME / "cache").mkdir(parents=True, exist_ok=True)
        shutil.copy(real_cache, HOME / "cache" / "models_cache.json")


def env(extra=None):
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["XDG_RUNTIME_DIR"] = str(RUN)
    for key in ("MIYU_DIRECT", "MIYU_SESSION", "MIYU_TURN_MODE", "XDG_CACHE_HOME", "XDG_CONFIG_HOME",
                "XDG_DATA_HOME", "XDG_STATE_HOME"):
        e.pop(key, None)
    e["LANG"] = "zh_CN.UTF-8"
    if extra:
        e.update(extra)
    return e


def find_socket():
    for p in RUN.rglob("*.sock"):
        return p
    return None


def cli(args, stdin=None, timeout=60):
    proc = subprocess.run([str(MIYU), *args], env=env(), input=stdin, capture_output=True, text=True, timeout=timeout)
    return proc.returncode, proc.stdout, proc.stderr


def json_lines(text):
    out = []
    for line in text.splitlines():
        line = line.strip()
        if line:
            out.append(json.loads(line))
    return out


def reply_summary(done):
    """done.text 是桩回的请求摘要 JSON。"""
    return json.loads(done["text"])


results = []


def check(name, ok, detail=""):
    results.append({"name": name, "ok": bool(ok), "detail": str(detail)[:300]})
    print(f"{'✅' if ok else '❌'} {name}  {detail}"[:400], flush=True)


def episodes_count():
    total = 0
    for db in HOME.rglob("memory.db"):
        try:
            con = sqlite3.connect(f"file:{db}?mode=ro&immutable=1", uri=True)
            total += con.execute("select count(*) from episodes").fetchone()[0]
            con.close()
        except sqlite3.Error:
            pass
    return total


def one_shot_scenarios():
    # 1. json 一行终态
    code, out, err = cli(["ask", "--output-format", "json", "TK hello"])
    lines = json_lines(out)
    ok = code == 0 and len(lines) == 1 and lines[0]["type"] == "done" and lines[0]["v"] == 1
    check("json: 单行 done,退出码 0", ok, f"code={code} lines={len(lines)} err={err.strip()[:80]}")
    done = lines[0] if lines else {}
    summary = reply_summary(done) if ok else {}
    check("json: done 带 session_id/usage/model/elapsed", ok and done.get("session_id") and done.get("usage")
          and done.get("model") == "stub-a" and "elapsed_ms" in done, json.dumps(done)[:200])
    check("json: 默认模型 stub-a、默认有工具、user_count=1",
          summary.get("model") == "stub-a" and summary.get("tools") and summary.get("user_count") == 1, summary)
    check("json: stderr 不混进 stdout", all(l.get("v") == 1 for l in lines))

    # 2. stream-json 事件序列
    code, out, err = cli(["ask", "--output-format", "stream-json", "TK hello"])
    lines = json_lines(out)
    types = [l["type"] for l in lines]
    check("stream-json: started → text… → done", code == 0 and types and types[0] == "started"
          and "text" in types and types[-1] == "done", types)
    deltas = "".join(l["delta"] for l in lines if l["type"] == "text")
    check("stream-json: delta 拼起来 == done.text", lines and deltas == lines[-1].get("text"))

    # 3. --model
    code, out, _ = cli(["ask", "--output-format", "json", "--model", "stub/stub-b", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--model stub/stub-b 生效", s.get("model") == "stub-b", s.get("model"))
    code, out, err = cli(["ask", "--output-format", "json", "--model", "nope/none", "TK hi"])
    check("--model 不存在 → 退出码 2", code == 2, f"code={code} err={err.strip()[:100]}")
    code, out, _ = cli(["--model", "2", "--output-format", "json", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("根命令 --model 序号 2 = stub-b", s.get("model") == "stub-b", s.get("model"))

    # 4. 提示词
    code, out, _ = cli(["ask", "--output-format", "json", "--system-prompt", "SYSX you are a translator", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--system-prompt 整体替换(system 以 SYSX 开头)", s.get("system_head", "").startswith("SYSX"), s.get("system_head"))
    prompt_file = OUT / "append.md"
    prompt_file.write_text("APPX from file", encoding="utf-8")
    code, out, _ = cli(["ask", "--output-format", "json", "--append-system-prompt", f"@{prompt_file}", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--append-system-prompt @文件 → <host-instructions>", s.get("host_instructions") == "APPX from file",
          s.get("host_instructions"))
    check("追加时人格提示词仍在(system 不以 APPX 开头)", not s.get("system_head", "").startswith("APPX"))
    code, out, err = cli(["ask", "--output-format", "json", "--append-system-prompt", "@/nonexistent.md", "TK hi"])
    check("@文件不存在 → 退出码 2", code == 2, f"code={code}")

    # 5. 工具面
    code, out, _ = cli(["ask", "--output-format", "json", "--no-tools", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--no-tools → tools 空", code == 0 and not s.get("tools"), s.get("tools"))
    code, out, _ = cli(["ask", "--output-format", "json", "--tools", "read,glob", "TK hi"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--tools read,glob → 只剩这两个", sorted(s.get("tools") or []) == ["glob", "read"], s.get("tools"))

    # 6. 会话:--create、历史延续、session 子命令
    code, out, err = cli(["ask", "--output-format", "json", "--session", "translate", "TK hi"])
    check("--session 不存在且无 --create → 退出码 3", code == 3, f"code={code} err={err.strip()[:80]}")
    code, out, _ = cli(["ask", "--output-format", "json", "--session", "translate", "--create", "TK first"])
    first = json_lines(out)[0] if code == 0 else {}
    check("--session translate --create 成功", code == 0 and first.get("type") == "done", code)
    code, out, _ = cli(["ask", "--output-format", "json", "--session", "translate", "TK second"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("第二轮同会话 user_count=2(历史延续)", s.get("user_count") == 2, s.get("user_count"))
    code, out, err = cli(["ask", "--output-format", "json", "--session", "translate", "--mode", "dev", "TK x"])
    check("对已有会话传 --mode → 退出码 2", code == 2, f"code={code} err={err.strip()[:100]}")
    code, out, _ = cli(["session", "list", "--json"])
    data = json.loads(out) if code == 0 else {}
    names = [s_["name"] for s_ in data.get("sessions", [])]
    check("session list --json 含 translate,不含阅后即焚", "translate" in names and "一次性对话" not in names
          and "One-shot" not in names, names)
    code, out, _ = cli(["session", "show", "translate", "--json"])
    detail = json.loads(out) if code == 0 else {}
    check("session show --json: turn_count=2、model_override 空", detail.get("turn_count") == 2
          and not detail.get("model_override"), json.dumps(detail, ensure_ascii=False)[:200])
    code, out, _ = cli(["ask", "--output-format", "json", "--session", "translate", "--context-window", "12345", "TK x"])
    d = json_lines(out)[0] if code == 0 else {}
    check("--context-window 12345 → done.context_window", d.get("context_window") == 12345, d.get("context_window"))
    code, out, _ = cli(["ask", "--output-format", "json", "--session", "translate", "--model", "stub/stub-b", "TK x"])
    code, out, _ = cli(["session", "show", "translate", "--json"])
    detail = json.loads(out) if code == 0 else {}
    check("--model 不落盘(session show 的 model_override 仍空)", not detail.get("model_override"), detail.get("model_override"))
    code, out, _ = cli(["session", "clear", "translate"])
    code, out, _ = cli(["ask", "--output-format", "json", "--session", "translate", "TK after clear"])
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("session clear 后 user_count=1", s.get("user_count") == 1, s.get("user_count"))
    code, out, _ = cli(["reset", "--session", "translate"])
    check("reset --session 认会话", code == 0, out.strip()[:60])
    code, out, _ = cli(["session", "rename", "translate", "trans2"])
    code, out, _ = cli(["session", "show", "trans2", "--json"])
    check("session rename 生效", code == 0 and json.loads(out).get("name") == "trans2", out.strip()[:80])
    code, out, _ = cli(["session", "new", "devsess", "--mode", "dev", "--json"])
    created = json.loads(out) if code == 0 else {}
    check("session new --mode dev → mode=dev", created.get("mode") == "dev", created)
    code, out, _ = cli(["session", "delete", "trans2", "--yes"])
    code2, out2, _ = cli(["session", "delete", "devsess", "--yes"])
    code, out, _ = cli(["session", "list", "--json"])
    names = [s_["name"] for s_ in json.loads(out).get("sessions", [])]
    check("session delete 后列表不含", "trans2" not in names and "devsess" not in names, names)

    # 7. --no-memory
    before = episodes_count()
    cli(["ask", "--output-format", "json", "--no-memory", "TK remember nothing"])
    time.sleep(1.0)
    after_no = episodes_count()
    cli(["ask", "--output-format", "json", "TK remember this"])
    time.sleep(1.0)
    after_yes = episodes_count()
    check("--no-memory 不写 episodes;默认写", after_no == before and after_yes > after_no,
          f"before={before} no_memory={after_no} default={after_yes}")

    # 8. --stdin 长输入
    big = "TK " + "x" * 120_000
    code, out, _ = cli(["ask", "--output-format", "json", "--stdin", "TK summarize"], stdin=big)
    s = reply_summary(json_lines(out)[0]) if code == 0 else {}
    check("--stdin 12 万字符不截断", s.get("last_user_len", 0) >= 120_000, s.get("last_user_len"))
    code, out, err = cli(["ask", "--output-format", "json", "--stdin", "TK summarize"], stdin="TK " + "y" * 250_000)
    check("--stdin 超 20 万字符 → 退出码 2", code == 2, f"code={code} err={err.strip()[:80]}")

    # 9. 超时与失败
    t0 = time.time()
    code, out, _ = cli(["ask", "--output-format", "json", "--timeout", "1", "TK SLOW please"])
    lines = json_lines(out)
    check("--timeout 1 → 退出码 124 + error.kind=timeout", code == 124 and lines and lines[0]["type"] == "error"
          and lines[0]["kind"] == "timeout", f"code={code} {time.time() - t0:.1f}s {out.strip()[:100]}")
    code, out, _ = cli(["ask", "--output-format", "json", "TK FAIL"])
    lines = json_lines(out)
    check("模型 500 → 退出码 1 + error.kind=turn_failed", code == 1 and lines and lines[0]["type"] == "error"
          and lines[0]["kind"] == "turn_failed", f"code={code} {out.strip()[:120]}")

    # 10. text 模式仍能带覆盖(--stdout 别名)
    code, out, _ = cli(["--stdout", "--model", "stub/stub-b", "TK hi"])
    check("text 模式 --stdout + --model", code == 0 and '"model": "stub-b"' in out, out.strip()[:120])


def stdio_scenarios():
    proc = subprocess.Popen([str(MIYU), "stdio"], env=env(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=open(OUT / "stdio.stderr", "w"), text=True, bufsize=1)
    events = []
    queue = Queue()

    def pump():
        for line in proc.stdout:
            queue.put(line)
        queue.put(None)

    threading.Thread(target=pump, daemon=True).start()

    def send(obj):
        proc.stdin.write(json.dumps(obj, ensure_ascii=False) + "\n")
        proc.stdin.flush()

    def read_until(pred, timeout=30):
        """带超时地读事件直到 pred 命中;超时返回 None(不再永远挂在 readline 上)。"""
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                return None
            try:
                line = queue.get(timeout=remaining)
            except Empty:
                return None
            if line is None:
                return None
            ev = json.loads(line)
            events.append(ev)
            if pred(ev):
                return ev

    ready = read_until(lambda e: e["type"] == "ready", 30)
    check("stdio: ready 事件", ready and ready.get("daemon_pid"), ready)
    send({"type": "ping", "id": "p1"})
    check("stdio: ping → pong", read_until(lambda e: e["type"] == "pong" and e["id"] == "p1", 5))

    # 并发两回合
    send({"type": "message", "id": "a", "content": "TK SLOW alpha"})
    send({"type": "message", "id": "b", "content": "TK beta"})
    done_b = read_until(lambda e: e["type"] == "done" and e["id"] == "b", 30)
    done_a = read_until(lambda e: e["type"] == "done" and e["id"] == "a", 30)
    check("stdio: 并发两回合各自 done,慢的后到", done_b and done_a
          and events.index(done_b) < events.index(done_a))
    ids_ok = all(e.get("id") in ("a", "b") for e in events if e["type"] in ("text", "started", "done"))
    check("stdio: 每条事件都带 id", ids_ok)

    # overrides 走同一套
    send({"type": "message", "id": "c", "content": "TK hi", "overrides": {"model": "stub-b", "no_tools": True}})
    done_c = read_until(lambda e: e["type"] == "done" and e["id"] == "c", 30)
    s = reply_summary(done_c) if done_c else {}
    check("stdio: overrides.model/no_tools 生效", s.get("model") == "stub-b" and not s.get("tools"), s)

    # question → answer
    send({"type": "message", "id": "q", "content": "TK ASK_QUESTION pick"})
    question = read_until(lambda e: e["type"] == "question" and e["id"] == "q", 30)
    check("stdio: question 事件带 question_id", question and question.get("question_id"), question)
    if question:
        send({"type": "answer", "id": "q", "question_id": question["question_id"], "answer": "蓝"})
    done_q = read_until(lambda e: e["type"] == "done" and e["id"] == "q", 30)
    check("stdio: answer 回到她手里(ANSWER:… 含 蓝)", done_q and done_q["text"].startswith("ANSWER:")
          and "蓝" in done_q["text"], done_q and done_q["text"][:120])

    # cancel
    send({"type": "message", "id": "z", "content": "TK SLOW zzz"})
    read_until(lambda e: e["type"] == "started" and e["id"] == "z", 10)
    send({"type": "cancel", "id": "z"})
    err_z = read_until(lambda e: e["type"] == "error" and e["id"] == "z", 15)
    check("stdio: cancel → error.kind=cancelled", err_z and err_z["kind"] == "cancelled", err_z)

    # session op + 会话延续 + create
    send({"type": "message", "id": "s1", "content": "TK one", "session": "host", "create": True})
    read_until(lambda e: e["type"] == "done" and e["id"] == "s1", 30)
    send({"type": "message", "id": "s2", "content": "TK two", "session": "host"})
    done_s2 = read_until(lambda e: e["type"] == "done" and e["id"] == "s2", 30)
    s = reply_summary(done_s2) if done_s2 else {}
    check("stdio: session+create 后历史延续 user_count=2", s.get("user_count") == 2, s.get("user_count"))
    send({"type": "session", "id": "l", "op": "list"})
    res = read_until(lambda e: e["type"] == "result" and e["id"] == "l", 10)
    names = [x["name"] for x in (res or {}).get("data", {}).get("sessions", [])]
    check("stdio: session op list 含 host", "host" in names, names)
    send({"type": "session", "id": "d", "op": "delete", "target": "host"})
    res = read_until(lambda e: e["type"] == "result" and e["id"] == "d", 10)
    check("stdio: session op delete ok", res and res.get("ok"))
    send({"type": "message", "id": "nf", "content": "TK x", "session": "ghost"})
    err = read_until(lambda e: e["type"] == "error" and e["id"] == "nf", 10)
    check("stdio: 会话不存在 → error.kind=session_not_found", err and err["kind"] == "session_not_found", err)
    send({"type": "bogus"})
    err = read_until(lambda e: e["type"] == "error" and e.get("id") is None, 10)
    check("stdio: 非法请求 → error.kind=usage", err and err["kind"] == "usage", err)

    # EOF:在跑的回合被取消,进程退出
    send({"type": "message", "id": "eof", "content": "TK SLOW bye"})
    read_until(lambda e: e["type"] == "started" and e["id"] == "eof", 10)
    proc.stdin.close()
    try:
        proc.wait(timeout=15)
        tail = []
        while True:
            try:
                line = queue.get(timeout=2)
            except Empty:
                break
            if line is None:
                break
            tail.append(json.loads(line))
        events.extend(tail)
        check("stdio: EOF 后退出,在跑回合收到 cancelled", proc.returncode == 0
              and any(e["type"] == "error" and e.get("id") == "eof" and e["kind"] == "cancelled" for e in tail),
              f"rc={proc.returncode} tail={[e['type'] for e in tail]}")
    except subprocess.TimeoutExpired:
        proc.kill()
        check("stdio: EOF 后退出", False, "did not exit in 15s")
    (OUT / "stdio-events.jsonl").write_text("\n".join(json.dumps(e, ensure_ascii=False) for e in events))


def main():
    assert MIYU.exists(), f"missing binary {MIYU}; run cargo build"
    build_home()
    stub = subprocess.Popen([sys.executable, str(BASE / "stub_llm.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT), STUB_LOG=str(OUT / "stub.jsonl")),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = subprocess.Popen([str(MIYU), "daemon", "--port", str(PORT)], env=env(),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    try:
        for _ in range(60):
            if find_socket():
                break
            time.sleep(0.5)
        assert find_socket(), "daemon socket never appeared"
        time.sleep(1.5)
        one_shot_scenarios()
        stdio_scenarios()
    finally:
        subprocess.run([str(MIYU), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        stub.terminate()
        (OUT / "verdict.json").write_text(json.dumps(results, ensure_ascii=False, indent=2), encoding="utf-8")
    passed = sum(1 for r in results if r["ok"])
    print(f"\n{passed}/{len(results)} passed")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
