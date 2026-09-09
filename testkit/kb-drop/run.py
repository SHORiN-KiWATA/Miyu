#!/usr/bin/env python3
"""知识库面板「拖放 + 多文件上传」真机走查(09-09)。

隔离 MIYU_HOME 起一个 daemon,用 Python playwright 开控制台 → 知识库面板,
合成真实的 DataTransfer 投放事件走一遍:拖拽指示层的出现与消失、拖文字不吃事件、
单文件 / 三文件 / 混合(含 png)/ 超限与非 UTF-8 / 被守卫拒绝 / 目录递归 / 选择器
多选,逐步截图并把控制台错误与非预期的 4xx-5xx 汇总成退出码。

    python3 testkit/kb-drop/run.py

前置:`cargo build`(web 静态资源 include_str! 进二进制,改了 JS/CSS 必须重新构建),
      以及 Python 的 playwright(chromium 在 ~/.cache/ms-playwright)。
坑:XDG_RUNTIME_DIR 路径太长会撞 SUN_LEN;daemon 一律用隐藏子命令 __daemon 起,
    别用 `miyu web`(它会去找线上 daemon);端口别碰 8300。
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-kb-drop/home"))
RUNTIME = "/tmp/mx-kbd"
PORT = int(os.environ.get("MIYU_KBD_PORT", "18477"))
SHOTS = Path(os.environ.get("MIYU_KBD_SHOTS", Path.home() / ".cache" / "miyu-kb-drop"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)


def write_config():
    """知识库开着、嵌入关掉:这条走查不碰模型(供应商只是配置必填项,不会被调用),
    也不该顺手起一个 ORT worker。"""
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{
            "id": "stub",
            "display_name": "Stub",
            "base_url": "http://127.0.0.1:1/v1",
            "protocol": "openai-chat",
            "api_key": "stub",
            "models": ["stub-model"],
        }],
        "plugins": {
            "knowledge_base": {"enabled": True, "embedding_enabled": False},
        },
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8"
    )


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


def main():
    if not BIN.exists():
        print(f"! 先 cargo build:{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists():
        shutil.rmtree(HOME)
    Path(RUNTIME).mkdir(exist_ok=True)
    write_config()
    SHOTS.mkdir(parents=True, exist_ok=True)

    daemon = subprocess.Popen(
        [str(BIN), "__daemon", "--port", str(PORT)],
        env=ENV, cwd=str(HOME),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        if not wait_http(f"{BASE}/api/config"):
            print("! daemon 没起来", file=sys.stderr)
            return 2
        print(f"· daemon {BASE}(home={HOME})")
        result = subprocess.run(
            [sys.executable, str(Path(__file__).parent / "shoot.py"), BASE, str(SHOTS)],
            cwd=str(REPO),
        )
        return result.returncode
    finally:
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=5)
        except subprocess.TimeoutExpired:
            daemon.kill()


if __name__ == "__main__":
    sys.exit(main())
