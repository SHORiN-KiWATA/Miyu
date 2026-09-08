#!/usr/bin/env python3
"""输入框提示的端到端验收(不花额度,只打 WebUI API)。

单元测试钉的是 `persona_identity` 的三态,这里钉的是**整条链**:
设置页 PUT /api/config → 人格元数据落盘 → GET /api/config 读回来。
风险都在这条链上——`PromptDocument` 带 `deny_unknown_fields`,前后端字段
不成对就整个请求 400(08-16 踩过一次)。

    python3 testkit/composer-placeholder/run.py
"""

import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-cph/home"))
WORK = Path(os.environ.get("MIYU_CPH_WORK", "/tmp/miyu-cph/work"))
PORT = int(os.environ.get("MIYU_CPH_PORT", "18396"))
RUNTIME = "/tmp/mx-cph"
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)


def daemon(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(BIN), "daemon", *args], env=ENV, cwd=WORK,
        capture_output=True, text=True, timeout=120,
    )


def api(path: str, body=None, method: str = "GET"):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(
        BASE + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Origin": BASE},
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read() or b"null")
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"{method} {path} → {error.code}: {error.read()[:400]!r}") from None


def main() -> int:
    HOME.mkdir(parents=True, exist_ok=True)
    WORK.mkdir(parents=True, exist_ok=True)
    Path(RUNTIME).mkdir(exist_ok=True)
    daemon("stop")
    started = daemon("start", "--port", str(PORT))
    if "运行中" not in (started.stdout + started.stderr) and started.returncode != 0:
        print((started.stdout + started.stderr)[-600:], file=sys.stderr)
        return 2
    failures = []
    try:
        time.sleep(3)
        snapshot = api("/api/config")
        config, prompts = snapshot["config"], snapshot["prompts"]

        # ① 没有激活人格时的出厂默认。
        placeholder = snapshot["persona"]["composer_placeholder"]
        print(f"① 出厂默认: {placeholder!r}")
        if placeholder != "给 Miyu 发消息":
            failures.append(f"① 期望「给 Miyu 发消息」，实得 {placeholder!r}")

        # ② 建一个自定义人格、不配提示 → 默认跟着人格名走。
        prompts["personas"] = [{"name": "小美.md", "content": "她叫小美。"}]
        config["prompt"]["active_persona"] = "小美.md"
        api("/api/config", {"config": config, "prompts": prompts, "secrets": {}}, "PUT")
        placeholder = api("/api/config")["persona"]["composer_placeholder"]
        print(f"② 换人格名后: {placeholder!r}")
        if placeholder != "给 小美 发消息":
            failures.append(f"② 期望「给 小美 发消息」，实得 {placeholder!r}")

        # ③ 配上自定义值 → 存得住、读得回。这一步同时验 deny_unknown_fields。
        snapshot = api("/api/config")
        config, prompts = snapshot["config"], snapshot["prompts"]
        prompts["personas"][0]["composer_placeholder"] = "有什么想说的？"
        api("/api/config", {"config": config, "prompts": prompts, "secrets": {}}, "PUT")
        placeholder = api("/api/config")["persona"]["composer_placeholder"]
        print(f"③ 配置自定义值: {placeholder!r}")
        if placeholder != "有什么想说的？":
            failures.append(f"③ 期望「有什么想说的？」，实得 {placeholder!r}")

        # 人格提示词与元数据落在 data/prompts/,不是 config/ 下(测具第一版找错了目录)。
        meta = HOME / "data" / "prompts" / "小美.json"
        stored = json.loads(meta.read_text("utf-8")) if meta.exists() else {}
        print(f"   元数据落盘 {meta}: {stored}")
        if stored.get("composer_placeholder") != "有什么想说的？":
            failures.append(f"③ 元数据没落盘: {stored}")

        # ④ 清空回落默认——空串不该被当成"配置过了"。元数据整个变空时那份
        # .json 会被删掉(所有字段都 None ⇒ 不留空壳文件),顺带钉住这一点。
        snapshot = api("/api/config")
        config, prompts = snapshot["config"], snapshot["prompts"]
        prompts["personas"][0]["composer_placeholder"] = "   "
        api("/api/config", {"config": config, "prompts": prompts, "secrets": {}}, "PUT")
        placeholder = api("/api/config")["persona"]["composer_placeholder"]
        print(f"④ 清空后回落: {placeholder!r}  元数据文件还在: {meta.exists()}")
        if placeholder != "给 小美 发消息":
            failures.append(f"④ 期望回落「给 小美 发消息」，实得 {placeholder!r}")
        if meta.exists():
            failures.append(f"④ 元数据已无内容却留了空壳文件: {meta.read_text('utf-8')!r}")
    finally:
        daemon("stop")

    for line in failures:
        print("FAIL " + line)
    print("\n全过" if not failures else f"\n{len(failures)} 项未过")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
