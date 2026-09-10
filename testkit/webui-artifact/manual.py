#!/usr/bin/env python3
"""手动验收用的沙箱:起一个隔离的 daemon + 桩模型,产出几份 artifact,然后把地址
打出来等你自己用浏览器点。

    BIN=<miyu 二进制> python3 testkit/webui-artifact/manual.py

跟生产完全隔离(自己的 MIYU_HOME、自己的端口),不碰你的会话、配置和记忆库。
Ctrl-C 收摊,沙箱目录留在 ~/.cache/miyu-artifact-manual 里。

自动化能测的都测过了(run.py),这里专门留给自动化测不了的那一项:
**人手点一下 probe.html 里那个按钮**,看文字变不变。Playwright 自带的 click
在不透明源 iframe 上不工作(它的可点性检查跨不了进程边界),所以这条只能人来。
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-artifact-manual")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18486"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18496"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-artifact"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-artifact"],
            "model_context_window": {"stub-artifact": 100000},
        }],
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


def api(method, path, payload=None):
    data = json.dumps(payload).encode() if payload is not None else None
    request = urllib.request.Request(
        BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(request, timeout=30) as response:
        raw = response.read()
        return json.loads(raw) if raw else {}


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_artifact.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "桩模型没起来"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon 没起来"
        time.sleep(1)
        api("POST", "/api/sessions", {"name": "artifact 验收", "switch": True})
        print("正在让她写几份文件…")
        try:
            api("POST", "/api/turns", {"content": "写几份探针文件到预览工作区"})
        except Exception:
            # 老一点的路由名不一样,失败就让你自己在界面里发一句。
            print("（没能自动发消息,打开页面后自己发一句「写几份探针文件」即可）")
        time.sleep(6)
        print()
        print("=" * 66)
        print(f"  打开：{BASE}")
        print("=" * 66)
        print("""
  右上角那个方块图标打开预览面板，下拉菜单里有五份文件：

    chart.html   ← 这张是重点：ECharts 画的柱状图 + 饼图，鼠标悬停看数值
    probe.html   ← 往下滚，F 那一格有个按钮，**用鼠标点它**
                    文字从 click me 变成 CLICKED 就说明交互真的活了
                    同一页里 G~M 七格都该写着 BLOCKED 或 SecurityError
    probe.svg    ← 该直接显示成图，左上角有预览/源码切换和缩放
    probe.csv    ← 该是一张表格，不是一堆逗号
    probe.md     ← 切到「查看源码」，该有语法配色（以前是纯白）

  看完 Ctrl-C 收摊。
""")
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\n收摊。")
    finally:
        for process in (daemon, stub):
            if process:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except Exception:
                    process.kill()


if __name__ == "__main__":
    main()
