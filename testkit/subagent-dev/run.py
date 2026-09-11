#!/usr/bin/env python3
"""subagent 工具黑盒实测(09-11 改名 + dev 模式)。

`miyu tool-call subagent '<json>'` 在 daemon 不在时本地执行同一条工具路径,
子代理的请求全落到桩 LLM 上——模型实际看到的系统提示词与工具面就是取证。

判据:

    1. 工具叫 subagent(旧名 task 已不存在)
    2. dev=false:系统提示词是通用子代理那份;工具面带记忆工具
    3. dev=true :系统提示词 = dev 提示词 + <host-environment> + <runtime cwd=> + 交付约定
       工具面是 core_only(run_command/apply_patch/edit 在,recall_memories/
       load_skill/use_meme 不在),且 subagent 自己不在面上(防递归)
    4. 两次 dev 调用的系统提示词逐字节相同(前缀缓存不被自己掰断)
    5. 工具循环真的跑起来:run_command 的输出回灌,最终结论带回调用方

用法:先 `cargo build`,再 `python3 testkit/subagent-dev/run.py`。
绝不触碰线上 8300 daemon(本测具压根不起 daemon)。产物在 out/。
"""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
MIYU = Path(os.environ.get("BIN") or REPO / "target" / "debug" / "miyu")
BASE = Path(__file__).resolve().parent
OUT = BASE / "out"
HOME = BASE / "home"
WORK = BASE / "work"
STUB_PORT = 18496
STUB_LOG = OUT / "stub.jsonl"

spec = importlib.util.spec_from_file_location(
    "persona_ab", REPO / "testkit" / "persona-ab" / "run.py"
)
persona_ab = importlib.util.module_from_spec(spec)
spec.loader.exec_module(persona_ab)


def build_home():
    for path in (HOME, OUT, WORK):
        if path.exists():
            shutil.rmtree(path)
    (HOME / "config").mkdir(parents=True)
    OUT.mkdir(parents=True)
    WORK.mkdir(parents=True)
    cfg = persona_ab.load_real_config()
    for key in ("platforms", "web", "voice", "alarm"):
        cfg.pop(key, None)
    cfg["providers"] = [
        {
            "enabled": True,
            "id": "stub",
            "display_name": "Stub",
            "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat",
            "api_key": "stub-key",
            "models": ["stub-a"],
        }
    ]
    cfg["active_provider_models"] = [{"provider_id": "stub", "model": "stub-a"}]
    cfg.pop("active_multimodal_provider_models", None)
    cfg.setdefault("prompt", {})["active_persona"] = ""
    memory = cfg.setdefault("memory", {})
    memory["enabled"] = True
    memory["association_enabled"] = False
    cfg.setdefault("cache", {})["request_log"] = False
    (HOME / "config" / "config.jsonc").write_text(
        json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def env():
    e = dict(os.environ)
    e["MIYU_HOME"] = str(HOME)
    for key in (
        "MIYU_DIRECT",
        "MIYU_SESSION",
        "MIYU_TURN_MODE",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ):
        e.pop(key, None)
    e["LANG"] = "zh_CN.UTF-8"
    return e


def cli(args, timeout=180):
    proc = subprocess.run(
        [str(MIYU), *args],
        env=env(),
        cwd=str(WORK),
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    return proc.returncode, proc.stdout, proc.stderr


def stub_records():
    if not STUB_LOG.exists():
        return []
    return [json.loads(line) for line in STUB_LOG.read_text().splitlines() if line.strip()]


def main():
    if not MIYU.exists():
        print(f"缺少二进制 {MIYU}，先 cargo build")
        return 1
    build_home()
    stub_env = dict(os.environ)
    stub_env["STUB_PORT"] = str(STUB_PORT)
    stub_env["STUB_LOG"] = str(STUB_LOG)
    stub = subprocess.Popen(
        [sys.executable, str(BASE / "stub.py")],
        env=stub_env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    time.sleep(1.0)
    checks = []

    def check(name, ok, detail=""):
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        print(("  [OK] " if ok else "  [NG] ") + name + (f" — {detail}" if detail else ""))

    try:
        # 1. 目录里只有新名
        rc, out, err = cli(["tool-call", "--list"])
        names = {line.split()[0] for line in out.splitlines() if line.strip()}
        check("工具面有 subagent", "subagent" in out, out[:120])
        check("旧名 task 已消失", "\ntask " not in out and not out.startswith("task "), "")

        # 2. 普通子代理
        base = len(stub_records())
        rc, out, err = cli(
            [
                "tool-call",
                "subagent",
                json.dumps(
                    {"description": "普通探针", "prompt": "TK probe normal", "max_steps": 4}
                ),
            ]
        )
        normal_out = out
        normal = stub_records()[base:]
        check("普通子代理跑通", rc == 0 and "SUBAGENT-DONE" in out, (err or out)[-200:])
        first_normal = normal[0] if normal else {}
        check(
            "普通子代理用通用子代理提示词",
            first_normal.get("system_head", "").startswith("你是通用任务子代理"),
            first_normal.get("system_head", "")[:60],
        )
        check(
            "普通子代理工具面带记忆工具",
            "recall_memories" in first_normal.get("tools", []),
            "",
        )

        # 3. dev 子代理
        base = len(stub_records())
        rc, out, err = cli(
            [
                "tool-call",
                "subagent",
                json.dumps(
                    {
                        "description": "dev 探针",
                        "prompt": "TK probe dev",
                        "dev": True,
                        "max_steps": 4,
                    }
                ),
            ]
        )
        dev_out = out
        dev = stub_records()[base:]
        check("dev 子代理跑通", rc == 0 and "SUBAGENT-DONE" in out, (err or out)[-200:])
        first_dev = dev[0] if dev else {}
        system = first_dev.get("system_full", "")
        tools = first_dev.get("tools", [])
        check(
            "dev 用开发模式提示词",
            system.startswith("You are a helpful software engineer assistant."),
            system[:60],
        )
        check("dev 带 host-environment", "<host-environment" in system, "")
        check("dev 带工作目录", f'<runtime cwd="{WORK}"' in system, system[-260:])
        check("dev 带交付约定", "goes back to the agent that delegated" in system, "")
        # 补丁编辑器注册出来的工具名是 edit(apply_patch 是模块名)。
        for expected in ("run_command", "edit", "goal", "todowrite", "job"):
            check(f"dev 工具面有 {expected}", expected in tools, ",".join(tools[:12]))
        for forbidden in ("recall_memories", "load_skill", "use_meme", "subagent", "task"):
            check(f"dev 工具面没有 {forbidden}", forbidden not in tools, ",".join(tools))
        check(
            "dev 里记忆工具确实调不动(探针回 unknown tool)",
            any("unknown tool" in (r.get("last_text") or "") for r in dev),
            "",
        )
        check(
            "run_command 在工作目录里执行",
            str(WORK) in dev_out,
            dev_out[-200:],
        )

        # 4. 两次 dev 的系统提示词逐字节相同
        base = len(stub_records())
        cli(
            [
                "tool-call",
                "subagent",
                json.dumps(
                    {
                        "description": "dev 探针二",
                        "prompt": "TK probe dev again",
                        "dev": True,
                        "max_steps": 4,
                    }
                ),
            ]
        )
        dev2 = stub_records()[base:]
        check(
            "两次 dev 的系统提示词逐字节相同",
            bool(dev2) and dev2[0].get("system_full") == system,
            "",
        )

        (OUT / "verdict.json").write_text(
            json.dumps(
                {
                    "checks": checks,
                    "normal_output": normal_out,
                    "dev_output": dev_out,
                    "dev_tools": tools,
                },
                ensure_ascii=False,
                indent=2,
            ),
            encoding="utf-8",
        )
    finally:
        stub.terminate()
        stub.wait(timeout=10)

    failed = [c for c in checks if not c["ok"]]
    print(f"\n{len(checks) - len(failed)}/{len(checks)} 通过")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
