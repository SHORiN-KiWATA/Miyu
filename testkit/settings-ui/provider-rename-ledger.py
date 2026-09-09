#!/usr/bin/env python3
"""供应商改 id → 用量账本跟着改名 的端到端走查（沙箱 daemon，不花额度）。

先造几行用量账本（provider = "alpha" 两行、"other" 一行 + 一行脏数据），
再走 WebUI 真正走的那条路 `PUT /api/config` 把 alpha 改成 "beta"，
然后看账本和 `/api/usage/stats` 有没有跟着改。

    python3 testkit/settings-ui/provider-rename-ledger.py

前置：`cargo build`。
"""

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-rename/home"))
RUNTIME = "/tmp/mx-rn"
PORT = int(os.environ.get("MIYU_RN_PORT", "18416"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)
LEDGER = HOME / "state" / "usage-history.jsonl"

failures = []


def check(name, ok, detail=""):
    print(f"{'  ok ' if ok else 'FAIL '} {name}{('  ' + str(detail)) if detail else ''}")
    if not ok:
        failures.append(name)


def api(path, body=None, method="GET"):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Origin": BASE},
    )
    with urllib.request.urlopen(request, timeout=120) as response:
        return json.loads(response.read() or b"null")


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "alpha",
        "active_provider_models": [{"provider_id": "alpha", "model": "m1"}],
        "providers": [
            {"id": "alpha", "display_name": "Alpha", "base_url": "http://127.0.0.1:9/v1",
             "protocol": "openai-chat", "api_key": "k", "models": ["m1"]},
            {"id": "other", "display_name": "Other", "base_url": "http://127.0.0.1:10/v1",
             "protocol": "openai-chat", "api_key": "k", "models": ["m2"]},
        ],
        "memory": {"enabled": False},
        "tools": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def seed_ledger():
    LEDGER.parent.mkdir(parents=True, exist_ok=True)
    now = int(time.time())
    rows = [
        {"ts": now - 300, "src": "agent", "provider": "alpha", "model": "m1",
         "prompt": 10, "completion": 5, "total": 15},
        {"ts": now - 200, "src": "qq", "provider": "alpha", "model": "m1",
         "prompt": 20, "completion": 6, "total": 26},
        {"ts": now - 100, "src": "agent", "provider": "other", "model": "m2",
         "prompt": 30, "completion": 7, "total": 37},
    ]
    body = "".join(json.dumps(row) + "\n" for row in rows) + "not json\n"
    LEDGER.write_text(body, encoding="utf-8")


def wait_http(url, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            time.sleep(0.2)
    return False


def providers_in_stats():
    """统计页的模型行挂在 sources[].models[] 下面（见 UsageStats/SourceUsage）。"""
    stats = api("/api/usage/stats?range=all")["stats"]
    return sorted({
        model.get("provider", "")
        for source in stats.get("sources", [])
        for model in source.get("models", [])
    })


def main():
    if not BIN.exists():
        print(f"! 先 cargo build：{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists():
        shutil.rmtree(HOME)
    Path(RUNTIME).mkdir(exist_ok=True)
    write_config()

    daemon = subprocess.Popen(
        [str(BIN), "__daemon", "--port", str(PORT)],
        env=ENV, cwd=str(HOME),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_http(f"{BASE}/api/config"):
            print("! daemon 没起来", file=sys.stderr)
            return 2
        seed_ledger()
        check("改名前统计里挂着 alpha", "alpha" in providers_in_stats(), providers_in_stats())

        current = api("/api/config")
        config = current["config"]
        for provider in config["providers"]:
            if provider["id"] == "alpha":
                provider["id"] = "beta"
        config["active_provider"] = "beta"
        config["active_provider_models"] = [{"provider_id": "beta", "model": "m1"}]
        api("/api/config", {"config": config, "prompts": current["prompts"], "secrets": {}}, "PUT")
        time.sleep(1.5)  # 账本重写走 spawn_blocking，等它落盘

        lines = LEDGER.read_text(encoding="utf-8").splitlines()
        parsed = [json.loads(line) for line in lines if line.startswith("{")]
        got = [row["provider"] for row in parsed]
        check("账本里 alpha 全改成 beta", got == ["beta", "beta", "other"], got)
        check("脏行原样留着", "not json" in lines, lines[-1])
        check("行数不变", len(lines) == 4, len(lines))
        after = providers_in_stats()
        check("统计里只剩 beta 和 other", after == ["beta", "other"], after)
        check("统计不再有 alpha", "alpha" not in after, after)

        # 再存一次（这次没改 id）不应该动账本。
        before = LEDGER.read_text(encoding="utf-8")
        current = api("/api/config")
        api("/api/config", {"config": current["config"], "prompts": current["prompts"], "secrets": {}}, "PUT")
        time.sleep(1.0)
        check("无关保存不动账本", LEDGER.read_text(encoding="utf-8") == before)
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
    print(f"\n断言失败 {len(failures)} 条" + (": " + ", ".join(failures) if failures else ""))
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
