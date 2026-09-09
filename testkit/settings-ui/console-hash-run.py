#!/usr/bin/env python3
"""起一个沙箱 daemon，跑 console-hash-shoot.js。

不花额度：模型指向一个不存在的地址，这两项走查全在前端，不发任何一轮对话。

    python3 testkit/settings-ui/console-hash-run.py            # 跑走查后自动收摊
    python3 testkit/settings-ui/console-hash-run.py --serve    # 只起 daemon，停住让人自己看

`--serve` 打印地址后一直挂着，Ctrl-C 收摊。想用真模型看（比如验目标续轮插话），
把自己的配置拷进沙箱再 --serve：

    MIYU_CH_SEED_CONFIG=~/.miyu/config/config.jsonc \\
      python3 testkit/settings-ui/console-hash-run.py --serve

沙箱只共用那一份 config，会话库、记忆、账本全是空的新家，碰不到 ~/.miyu。

前置：`cargo build`（web/*.js 与 styles.css 编进二进制），
      以及该目录下的 playwright（node_modules 已在；走查才需要，--serve 不需要）。
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
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-console-hash/home"))
RUNTIME = "/tmp/mx-ch"  # 路径要短，否则撞 unix socket 的 SUN_LEN
PORT = int(os.environ.get("MIYU_CH_PORT", "18412"))
SHOTS = Path(os.environ.get("MIYU_CH_SHOTS", Path.home() / ".cache" / "miyu-console-hash"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    # 想用真模型看的话，把自己的 config 拷进来（只拷这一份，其余全是空的新家）。
    seed = os.environ.get("MIYU_CH_SEED_CONFIG")
    if seed:
        shutil.copyfile(Path(seed).expanduser(), HOME / "config" / "config.jsonc")
        return
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-model"}],
        "providers": [{
            "id": "stub",
            "display_name": "Stub",
            "base_url": "http://127.0.0.1:9/v1",
            "protocol": "openai-chat",
            "api_key": "stub",
            "models": ["stub-model"],
        }],
        "memory": {"enabled": False},
        "tools": {"enabled": False},
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
    serve = "--serve" in sys.argv[1:]
    if not BIN.exists():
        print(f"! 先 cargo build：{BIN} 不存在", file=sys.stderr)
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
        if serve:
            print(f"沙箱 WebUI: {BASE}")
            print(f"沙箱 home:  {HOME}")
            print(f"日志:      {HOME / 'state' / 'logs'}（要 info 级加 MIYU_LOG=info 重跑）")
            print("Ctrl-C 收摊（沙箱目录会留着，下次跑会重建）")
            try:
                daemon.wait()
            except KeyboardInterrupt:
                pass
            return 0
        # playwright 装在主检出的 testkit/settings-ui/node_modules（不入库）；
        # 在 worktree 里跑时要指回去。
        modules = [HERE / "node_modules"]
        if os.environ.get("MIYU_MAIN_CHECKOUT"):
            modules.append(Path(os.environ["MIYU_MAIN_CHECKOUT"]) / "testkit" / "settings-ui" / "node_modules")
        node_env = dict(os.environ, NODE_PATH=os.pathsep.join(str(p) for p in modules))
        return subprocess.call(
            ["node", str(HERE / "console-hash-shoot.js"), BASE, str(SHOTS)],
            cwd=str(HERE), env=node_env,
        )
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()


if __name__ == "__main__":
    sys.exit(main())
