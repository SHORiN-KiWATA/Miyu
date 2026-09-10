#!/usr/bin/env python3
"""「做出来她会用吗」——拿真模型跑,不是桩。

    BIN=<miyu 二进制> PROVIDER=bigmodel MODEL=glm-5.3-flash \
        python3 testkit/webui-artifact/does_she_use_it.py

从你真实的 config.jsonc 里挑一个供应商搬进沙箱 MIYU_HOME(**不碰生产 daemon、
不碰你的会话和记忆**),然后像平常那样跟她说两句话,看她自己会不会想到:

  一、数据题:给一串数字,她会画图还是甩一张 markdown 表?
  二、结构题:要一张流程图,她会怎么画?(这题直接决定 Mermaid 那 3.4MB 要不要花)

判的不是图好不好看,是**她想不想得起来用这个能力**——能力再强,她想不起来就等于没有。
产出:~/.cache/miyu-does-she-use-it/report.json,含她每轮用的工具和写出来的文件全文。
"""
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

BIN = Path(os.environ["BIN"]).expanduser()
PROVIDER = os.environ.get("PROVIDER", "bigmodel")
MODEL = os.environ.get("MODEL", "glm-5.3-flash")
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-does-she-use-it")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18489"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))

# 第一轮的「数据题」问法有毛病:说「记一下」把她带去了记账工具,压根没到画图那一步。
# 第二轮改成明确要图,专门验一件事——ECharts 内置了,她会用吗,还是照样手搓 SVG。
TASK_SETS = {
    "1": [
        ("数据题",
         "这周的开销记一下：周一 120，周二 340，周三 86，周四 255，周五 410。帮我看看花在哪了。"),
        ("结构题",
         "画一张图说明你处理一次对话的流程：我发消息 → daemon 收下 → 送给模型 → 模型要调工具 "
         "→ 工具结果回灌给模型 → 模型给出回复。"),
    ],
    "2": [
        ("柱状图",
         "把这五个数画成柱状图给我看：周一 120，周二 340，周三 86，周四 255，周五 410。"),
        ("占比图",
         "再画个饼图看占比：餐饮 1180，交通 620，购物 480，娱乐 340，其他 227。"),
    ],
    "3": [
        ("结构题复跑",
         "画一张图说明你处理一次对话的流程：我发消息 → daemon 收下 → 送给模型 → 模型要调工具 "
         "→ 工具结果回灌给模型 → 模型给出回复。"),
    ],
}
TASKS = TASK_SETS[os.environ.get("TASK_SET", "1")]


def load_provider():
    for candidate in ("~/.config/miyu/config.jsonc", "~/.miyu/config/config.jsonc"):
        path = Path(candidate).expanduser()
        if path.exists():
            raw = re.sub(r"^\s*//.*$", "", path.read_text(encoding="utf-8"), flags=re.M)
            config = json.loads(raw)
            for provider in config.get("providers", []):
                if provider.get("id") == PROVIDER:
                    return provider
    raise SystemExit(f"配置里没找到供应商 {PROVIDER}")


def write_config(provider):
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": PROVIDER,
        "active_provider_models": [{"provider_id": PROVIDER, "model": MODEL}],
        "providers": [provider],
        # 记忆关掉:这次只看她会不会用工具,别把测试内容写进任何库。
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


def api(method, path, payload=None, timeout=300):
    data = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(
        BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        raw = response.read()
        return json.loads(raw) if raw else {}


def artifact_files(session_id):
    root = HOME / "state" / "artifacts" / session_id
    if not root.exists():
        root = HOME / "data" / "artifacts" / session_id
    if not root.exists():
        return {}
    out = {}
    for path in sorted(root.iterdir()):
        if path.is_file():
            try:
                out[path.name] = path.read_text(encoding="utf-8", errors="replace")
            except Exception as exc:
                out[path.name] = f"<读不出来: {exc}>"
    return out


def main():
    provider = load_provider()
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config(provider)
    report = {"provider": PROVIDER, "model": MODEL, "rounds": []}
    daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                              stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
    try:
        assert wait_http(f"{BASE}/api/config"), "daemon 没起来"
        time.sleep(1)
        for label, prompt in TASKS:
            created = api("POST", "/api/sessions", {"name": label, "switch": True})
            session_id = (created.get("session_id") or created.get("id")
                          or (created.get("session") or {}).get("session_id"))
            print(f"--- {label} --- {prompt[:30]}…")
            started = time.time()
            try:
                api("POST", "/api/turns", {"content": prompt, "session_id": session_id})
            except Exception as exc:
                print(f"  发消息失败：{exc}")
            # 回合是异步跑的,轮询到助手正文落库为止。
            turns = []
            while time.time() - started < 420:
                time.sleep(5)
                try:
                    turns = (api("GET", f"/api/sessions/{session_id}/turns") or {}).get("turns", [])
                except Exception:
                    continue
                if turns and turns[-1].get("assistant_content"):
                    break
            last = turns[-1] if turns else {}
            tools = []
            for round_ in last.get("tool_flow") or []:
                for call in round_.get("calls") or []:
                    tools.append(call.get("name") or call.get("tool") or "?")
            report["rounds"].append({
                "task": label,
                "prompt": prompt,
                "seconds": round(time.time() - started, 1),
                "tools": tools,
                "reply": (last.get("assistant_content") or "")[:1500],
                "artifacts": artifact_files(session_id),
            })
            print(f"  用了工具：{tools or '（一个没用）'}")
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=5)
        except Exception:
            daemon.kill()
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), "utf-8")
    print()
    for round_ in report["rounds"]:
        names = list(round_["artifacts"].keys())
        print(f"[{round_['task']}] {round_['seconds']}s 工具={round_['tools']} 产出文件={names}")
    print(f"\n全文在 {OUT}/report.json")


if __name__ == "__main__":
    main()
