#!/usr/bin/env python3
"""知识库「重建语义索引」的真机走查(09-09)。

复现的是用户实拍的那一幕:把一整份 wiki 拖进 WebUI 知识库,点「重建语义索引」,
待重建仍是「未索引 N」、重建卡立刻回到「空闲」、全程没有任何进度。

    python3 testkit/kb-reindex/run.py --files 400 --second-batch 100 [--shoot]

根因是「一趟重建在开跑那一刻就把文件清单定死了」:那趟跑着的时候点重建,请求
被一句 running 打发掉就此蒸发,新传进来的文件永远没人管;等那趟跑完锁一清,
卡片又回到「空闲」,界面上一点痕迹都没有。所以这条走查刻意分两批传:

  1. 传第一批 → 点重建(必须 started=true,且**下一帧就是进行中**,不是空闲)
  2. 第一趟正跑着 → 传第二批并再点一次(必须 reason=queued)
  3. 盯时间线:total 会从「第一批的清单」涨到「含第二批的清单」= 重跑生效
  4. 收尾断言:未索引归零、没有失败

`--shoot` 再用 playwright 走一遍浏览器侧(拖放 → 进度条 → 完成),截图落在
`~/.cache/miyu-kb-reindex/`。

前置:`cargo build`(web 静态资源编进二进制)、本机装了 onnxruntime 与内置
bge-small-zh 模型(`miyu embed status` 能探通)。端口默认 18436,别碰 8300。
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
BIN = Path(os.environ.get("MIYU_BIN", REPO / "target" / "debug" / "miyu"))
HOME = Path(os.environ.get("MIYU_HOME", "/tmp/miyu-kb-reindex/home"))
LIBRARY = Path(os.environ.get("MIYU_KBR_LIB", "/tmp/miyu-kb-reindex/library"))
RUNTIME = "/tmp/mx-kbr"
SHOTS = Path(os.environ.get("MIYU_KBR_SHOTS", Path.home() / ".cache" / "miyu-kb-reindex"))


def api(base, path, body=None, method="GET", timeout=120):
    data = json.dumps(body).encode() if body is not None else None
    request = urllib.request.Request(
        base + path, data=data, method=method,
        headers={"Content-Type": "application/json", "Origin": base},
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return json.loads(response.read() or b"null")


def upload(base, name, payload):
    request = urllib.request.Request(
        f"{base}/api/dash/kb/files?name={urllib.parse.quote(name)}",
        data=payload, method="POST",
        headers={"Content-Type": "application/octet-stream", "Origin": base},
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.loads(response.read())


def write_config():
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
        # 语义索引是这条走查的主角,必须真开着(本地内置模型)。
        "plugins": {"knowledge_base": {"enabled": True, "embedding_enabled": True}},
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8")


def wait_http(url, timeout=60):
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


def read_progress():
    try:
        return json.loads((HOME / "data" / "kb" / "embedding-reindex.json").read_text())
    except Exception:
        return {}


def snapshot(base):
    """面板每次刷新拿到的两样东西:概览 + 重建状态。"""
    overview = api(base, "/api/dash/kb/overview")
    status = api(base, "/api/dash/kb/reindex")
    return overview, status


def line(elapsed, overview, status):
    return (f"  t+{elapsed:6.1f}s  文件 {overview['file_count']:>5}"
            f"  语义块 {overview['semantic_chunks']:>6}"
            f"  未索引 {overview['unindexed_files']:>5}"
            f"  running={str(status.get('running')):<5}"
            f"  done={status.get('done')}/{status.get('total')}"
            f"  err={(status.get('last_error') or '')[:48]}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--files", type=int, default=400)
    parser.add_argument("--port", type=int, default=int(os.environ.get("MIYU_KBR_PORT", "18436")))
    parser.add_argument("--budget", type=float, default=900.0, help="重建等待上限(秒)")
    parser.add_argument("--keep-home", action="store_true", help="复用上次的 MIYU_HOME")
    parser.add_argument("--second-batch", type=int, default=100,
                        help="第一趟重建跑着的时候再传这么多个(根因就在这里)")
    parser.add_argument("--shoot", action="store_true", help="跑 playwright 截图")
    args = parser.parse_args()
    base = f"http://127.0.0.1:{args.port}"
    env = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=RUNTIME)

    if not BIN.exists():
        print(f"! 先 cargo build:{BIN} 不存在", file=sys.stderr)
        return 2
    if HOME.exists() and not args.keep_home:
        shutil.rmtree(HOME)
    HOME.mkdir(parents=True, exist_ok=True)
    Path(RUNTIME).mkdir(parents=True, exist_ok=True)
    SHOTS.mkdir(parents=True, exist_ok=True)
    total_seed = args.files + args.second_batch
    if not LIBRARY.exists() or len([p for p in LIBRARY.rglob("*") if p.is_file()]) < total_seed:
        subprocess.run([sys.executable, str(Path(__file__).parent / "seed_library.py"),
                        str(LIBRARY), "--count", str(total_seed)], check=True)

    # 走真实的首次启动:没有 config 时任何子命令都会跑 run_init,内置库在那儿导入,
    # 并顺手排一次后台重建。用户那台机器上的 5986 个语义块就是这么来的。
    if not (HOME / "config" / "config.jsonc").exists():
        subprocess.run([str(BIN), "kb", "stats"], env=env, cwd=str(HOME),
                       stdout=subprocess.DEVNULL, check=False)
        # 必须等那趟落定再改配置:配置正被改写的一瞬间读到的是空文件,子进程当场
        # 死掉(第一次跑这个测具就这么翻的车,顺带证明了失败现在真的留痕)。
        deadline = time.time() + 900
        while time.time() < deadline:
            progress = read_progress()
            if progress.get("phase") in ("done", "failed"):
                break
            time.sleep(2)
        print(f"· 首启那趟:{json.dumps(read_progress(), ensure_ascii=False)}")
    write_config()

    daemon = subprocess.Popen(
        [str(BIN), "__daemon", "--port", str(args.port)],
        env=env, cwd=str(HOME),
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    failures = []
    try:
        if not wait_http(f"{base}/api/config"):
            print("! daemon 没起来", file=sys.stderr)
            return 2
        print(f"· daemon {base}(home={HOME})")
        overview, status = snapshot(base)
        print(f"· 内置库就位:{overview['file_count']} 个文件,语义块 {overview['semantic_chunks']}"
              f",未索引 {overview['unindexed_files']}")

        seeded = sorted(path for path in LIBRARY.rglob("*") if path.is_file())
        first, second = seeded[: args.files], seeded[args.files : args.files + args.second_batch]

        def send(batch):
            for path in batch:
                upload(base, f"library/{path.relative_to(LIBRARY).as_posix()}", path.read_bytes())

        started = time.time()
        send(first)
        print(f"· 上传第一批 {len(first)} 个文件,用时 {time.time() - started:.1f}s")

        before, before_status = snapshot(base)
        print(f"· 上传后:文件 {before['file_count']}、语义块 {before['semantic_chunks']}、"
              f"未索引 {before['unindexed_files']}")
        if before["unindexed_files"] < len(first):
            failures.append("上传后未索引数不对,种子库没进去?")

        result = api(base, "/api/dash/kb/reindex", body={}, method="POST")
        click_at = time.time()
        print(f"· 点重建:{json.dumps(result, ensure_ascii=False)}")
        if not result.get("started"):
            failures.append(f"这一下应该真起一趟,却回了 {result}")
        # 用户看到的第一帧:前端 POST 完立刻 loadOverview。
        first_overview, first_status = snapshot(base)
        print("· 点完立刻刷新(用户看到的第一帧):")
        print(line(time.time() - click_at, first_overview, first_status))
        if not first_status.get("running"):
            failures.append("点完立刻刷新时 running=false —— 面板会显示「空闲」")

        # 根因现场:第一趟正跑着,再传一批。那趟的文件清单在开跑时就定死了,
        # 这一批它不知道;这次重建请求必须被排队,而不是原地蒸发。
        send(second)
        queued = api(base, "/api/dash/kb/reindex", body={}, method="POST")
        print(f"· 跑着的时候又传 {len(second)} 个并再点一次:"
              f"{json.dumps(queued, ensure_ascii=False)}")
        if queued.get("started") or queued.get("reason") != "queued":
            failures.append(f"第二批的重建请求没排上队:{queued}")

        print("· 时间线:")
        last_print = 0.0
        finished = False
        while time.time() - click_at < args.budget:
            overview, status = snapshot(base)
            elapsed = time.time() - click_at
            if elapsed - last_print >= 5 or not status.get("running"):
                print(line(elapsed, overview, status))
                last_print = elapsed
            if not status.get("running"):
                finished = True
                break
            time.sleep(0.5)
        overview, status = snapshot(base)
        print(f"· 结束:{json.dumps(status, ensure_ascii=False)}")
        if not finished:
            failures.append(f"{args.budget}s 内没跑完")
        if overview["unindexed_files"] != 0:
            failures.append(f"跑完还剩 {overview['unindexed_files']} 个未索引")
        if status.get("last_error"):
            failures.append(f"重建报错:{status['last_error']}")

        if args.shoot:
            shot = subprocess.run(
                [sys.executable, str(Path(__file__).parent / "shoot.py"), base, str(SHOTS)],
                cwd=str(REPO))
            if shot.returncode != 0:
                failures.append("截图/浏览器断言没过")
    finally:
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()

    for problem in failures:
        print(f"FAIL {problem}")
    print("走查通过" if not failures else f"走查失败:{len(failures)} 项")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
