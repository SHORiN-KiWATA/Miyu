#!/usr/bin/env python3
"""真宿主 demo:一个「以 Miyu 为 AI 后端的翻译软件」长什么样,并用真实供应商跑一遍。

视角是宿主开发者:起一个 `miyu stdio` 常驻,建一个专用会话,每次请求带同一段
追加指令、不写记忆、不给工具、指定模型;流式收正文,收 usage 看缓存。

隔离 home(拷真实 config,去掉平台/web/语音/闹钟)+ 独立端口 daemon,不碰线上
8300。真实供应商真实请求,会花钱。

用法:python3 testkit/cli/demo_host.py [--model provider/model]
产物:testkit/cli/out-demo/{events.jsonl,daemon.log,verdict.json}
"""
import argparse
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path
from queue import Empty, Queue

REPO = Path(__file__).resolve().parents[2]
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
BASE = Path(__file__).resolve().parent
OUT = BASE / "out-demo"
HOME = BASE / "home-demo"
RUN = Path.home() / ".cache" / "miyu-cli-demo-run"
PORT = 18396

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
    cfg.setdefault("cache", {})["request_log"] = False
    (HOME / "config" / "config.jsonc").write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    real_cache = Path.home() / ".miyu" / "cache" / "models_cache.json"
    if real_cache.exists():
        (HOME / "cache").mkdir(parents=True, exist_ok=True)
        shutil.copy(real_cache, HOME / "cache" / "models_cache.json")


def env():
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    e["XDG_RUNTIME_DIR"] = str(RUN)
    for key in ("MIYU_DIRECT", "MIYU_SESSION", "MIYU_TURN_MODE"):
        e.pop(key, None)
    return e


def find_socket():
    for p in RUN.rglob("*.sock"):
        return p
    return None


class MiyuBackend:
    """宿主软件里的「Miyu 后端」封装:一个 stdio 进程,按 id 分发事件。"""

    def __init__(self):
        self.proc = subprocess.Popen([str(MIYU), "stdio"], env=env(), stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=open(OUT / "stdio.stderr", "w"),
                                     text=True, bufsize=1)
        self.queue = Queue()
        self.events = []
        self.counter = 0
        threading.Thread(target=self._pump, daemon=True).start()
        ready = self.wait(lambda e: e["type"] == "ready", 60)
        assert ready, "no ready event"

    def _pump(self):
        for line in self.proc.stdout:
            self.queue.put(line)
        self.queue.put(None)

    def send(self, obj):
        self.proc.stdin.write(json.dumps(obj, ensure_ascii=False) + "\n")
        self.proc.stdin.flush()

    def wait(self, pred, timeout):
        deadline = time.time() + timeout
        while True:
            remaining = deadline - time.time()
            if remaining <= 0:
                return None
            try:
                line = self.queue.get(timeout=remaining)
            except Empty:
                return None
            if line is None:
                return None
            ev = json.loads(line)
            self.events.append(ev)
            (OUT / "events.jsonl").open("a", encoding="utf-8").write(json.dumps(ev, ensure_ascii=False) + "\n")
            if pred(ev):
                return ev

    def chat(self, session, content, overrides, timeout=120):
        """宿主的一次调用:流式打印正文,返回 (done, usage 事件列表)。"""
        self.counter += 1
        rid = f"req{self.counter}"
        self.send({"type": "message", "id": rid, "session": session, "create": True, "content": content,
                   "overrides": overrides, "timeout": timeout})
        usages = []
        text = []
        started = time.time()
        while True:
            ev = self.wait(lambda e: e.get("id") == rid, timeout + 10)
            if ev is None:
                return None, usages
            if ev["type"] == "text":
                text.append(ev["delta"])
                print(ev["delta"], end="", flush=True)
            elif ev["type"] == "usage":
                usages.append(ev)
            elif ev["type"] in ("done", "error"):
                print(f"\n  [{ev['type']} {time.time() - started:.1f}s]", flush=True)
                return ev, usages

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=15)


def cache_fields(usage):
    u = usage or {}
    details = u.get("prompt_tokens_details") or {}
    return {
        "prompt": u.get("prompt_tokens"),
        "cache_hit": u.get("prompt_cache_hit_tokens", details.get("cached_tokens")),
        "cache_miss": u.get("prompt_cache_miss_tokens"),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", default="deepseek/deepseek-v4-flash")
    parser.add_argument("--skip-default-pool", action="store_true")
    args = parser.parse_args()
    assert MIYU.exists(), f"missing binary {MIYU}"
    build_home()
    daemon = subprocess.Popen([str(MIYU), "daemon", "--port", str(PORT)], env=env(),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    verdict = {"model": args.model, "turns": []}
    backend = None
    try:
        for _ in range(60):
            if find_socket():
                break
            time.sleep(0.5)
        assert find_socket(), "daemon socket never appeared"
        time.sleep(1.5)
        backend = MiyuBackend()
        print("backend ready:", backend.events[0])

        # 翻译软件的固定配置:同一段追加指令、不写记忆、不给工具、指定模型。
        overrides = {
            "model": args.model,
            "append_system_prompt": "You are the translation backend of a desktop app. Reply with the translation only, no commentary.",
            "no_memory": True,
            "no_tools": True,
        }
        session = "demo-translator"
        prompts = ["把这句翻成日语:今天天气不错,我们去公园散步吧。",
                   "把上一句再翻成英语。",
                   "把上面两句的日语版和英语版并排列出来。"]
        for prompt in prompts:
            print(f"\n> {prompt}")
            done, usages = backend.chat(session, prompt, overrides)
            record = {"prompt": prompt, "type": done and done["type"], "elapsed_ms": done and done.get("elapsed_ms"),
                      "text": done and done.get("text"), "model": done and done.get("model"),
                      "usage": [cache_fields(u.get("usage")) for u in usages],
                      "done_usage": cache_fields(done.get("usage")) if done else None}
            verdict["turns"].append(record)
            print("  usage:", record["done_usage"])

        # 会话管理面:宿主看得到自己的会话。
        backend.send({"type": "session", "id": "show", "op": "show", "target": session})
        shown = backend.wait(lambda e: e.get("id") == "show", 15)
        verdict["session_show"] = shown and shown.get("data")
        print("session show:", json.dumps(verdict["session_show"], ensure_ascii=False)[:300])

        if not args.skip_default_pool:
            # 对照:不带任何覆盖,走配置里的默认池(带人格、带工具)。
            print("\n> [默认池] 用一句话介绍你自己")
            done, usages = backend.chat("demo-default", "用一句话介绍你自己。", {"no_memory": True}, timeout=180)
            verdict["default_pool"] = {"type": done and done["type"], "model": done and done.get("model"),
                                       "text": done and done.get("text"), "message": done and done.get("message")}
        backend.send({"type": "session", "id": "del1", "op": "delete", "target": session})
        backend.wait(lambda e: e.get("id") == "del1", 15)
        backend.send({"type": "session", "id": "del2", "op": "delete", "target": "demo-default"})
        backend.wait(lambda e: e.get("id") == "del2", 15)
    finally:
        if backend:
            try:
                backend.close()
            except Exception:
                backend.proc.kill()
        subprocess.run([str(MIYU), "daemon", "stop"], env=env(), capture_output=True, timeout=30)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        (OUT / "verdict.json").write_text(json.dumps(verdict, ensure_ascii=False, indent=2), encoding="utf-8")
    print("\nverdict written to", OUT / "verdict.json")


if __name__ == "__main__":
    main()
