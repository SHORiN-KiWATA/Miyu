#!/usr/bin/env python3
"""`miyu pm` 端到端(真二进制,隔离 MIYU_HOME,离线:只用本地路径来源)。

BIN=<miyu> python3 testkit/pm/pm_e2e.py

  1. 装扩展包(脚本 + 技能)→ extensions/ 里有文件、锁文件有记录、`tool-call --list` 能列出脚本
  2. 再装同一包 → 已是最新;改脚本内容后 upgrade → 文件更新
  3. 装人格包 → data/prompts/<name>.md、personas/<scope>/persona.toml、头像、人格专属脚本
  4. `miyupm list`(符号链接 shim)与 `miyu pm list` 一致
  5. 冲突:另一个包装同名脚本被拒
  6. 卸载:文件与锁记录都没了;tap list 至少有官方 tap
"""
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
BIN = Path(os.environ["BIN"]).expanduser()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-pm")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME),
           MIYU_SYSTEM_SCRIPTS_DIR=str(REPO / "src/scripts"), MIYU_ADMIN_USER="admin")

results = []


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  [{detail}]" if detail else ""), flush=True)


def run(*args, binary=None):
    proc = subprocess.run([str(binary or BIN), *args], env=ENV, cwd=str(OUT), capture_output=True, text=True, timeout=120)
    return proc.returncode, (proc.stdout + proc.stderr).strip()


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    work = OUT / "packages"
    shutil.copytree(HERE / "sample-ext", work / "sample-ext")
    shutil.copytree(HERE / "sample-persona", work / "sample-persona")

    # 1. 装扩展包
    code, out = run("pm", "install", str(work / "sample-ext"), "-y")
    check("pm install 本地扩展包", code == 0 and "sample-ext" in out, out[-200:])
    check("脚本落在 extensions/scripts", (HOME / "extensions/scripts/pm_hello.py").is_file())
    check("技能落在 extensions/skills", (HOME / "extensions/skills/pm-greeter/SKILL.md").is_file())
    lock = json.loads((HOME / "extensions/pm/lock.json").read_text())
    check("锁文件记录了包与文件", "sample-ext" in lock.get("packages", {}) and len(lock["packages"]["sample-ext"]["files"]) == 2,
          json.dumps(lock)[:160])
    code, out = run("tool-call", "--list")
    check("tool-call --list 列出 pm_hello", code == 0 and "pm_hello" in out, out[:200])
    code, out = run("tool-call", "pm_hello", '{"name":"pm"}')
    check("脚本能被调用", code == 0 and "hello, pm" in out, out[-160:])

    # 2. 重装 / 升级
    code, out = run("pm", "upgrade", "sample-ext", "-y")
    check("同内容 upgrade 报已是最新", code == 0 and ("最新" in out or "up to date" in out), out[-120:])
    script = work / "sample-ext/scripts/pm_hello.py"
    script.write_text(script.read_text().replace("(from sample-ext)", "(from sample-ext v2)"))
    (work / "sample-ext/miyu-package.toml").write_text((work / "sample-ext/miyu-package.toml").read_text().replace('version = "1.0.0"', 'version = "1.1.0"'))
    code, out = run("pm", "upgrade", "sample-ext", "-y")
    check("内容变了 upgrade 重装", code == 0 and "1.1.0" in out, out[-160:])
    check("升级后脚本是新内容", "v2" in (HOME / "extensions/scripts/pm_hello.py").read_text())

    # 3. 人格包
    code, out = run("pm", "install", str(work / "sample-persona"), "-y")
    check("pm install 人格包", code == 0, out[-200:])
    check("人格提示词落在 data/prompts", (HOME / "data/prompts/sample-persona.md").is_file())
    check("人格清单落在 personas/<scope>", (HOME / "personas/sample-persona-md/persona.toml").is_file())
    check("头像落在 persona-avatars/<name>", (HOME / "data/persona-avatars/sample-persona/avatar.png").is_file())
    check("人格专属脚本落在 extensions/scripts/personas/<scope>",
          (HOME / "extensions/scripts/personas/sample-persona-md/sample_mood.py").is_file())

    # 4. shim
    shim = OUT / "miyupm"
    shim.symlink_to(BIN)
    code, out = run("list", binary=shim)
    code2, out2 = run("pm", "list")
    check("miyupm 与 miyu pm 输出一致", code == 0 and code2 == 0 and out == out2 and "sample-ext" in out and "sample-persona" in out, out[:200])

    # 5. 冲突
    shutil.copytree(work / "sample-ext", work / "clash")
    (work / "clash/miyu-package.toml").write_text('[package]\nname = "clash"\n')
    code, out = run("pm", "install", str(work / "clash"), "-y")
    check("同名脚本被别的包占着 → 拒装", code != 0 and "sample-ext" in out, out[-160:])

    # 6. 卸载 / tap
    code, out = run("pm", "remove", "sample-ext")
    check("pm remove 扩展包", code == 0, out[-120:])
    check("卸载后脚本与技能都没了", not (HOME / "extensions/scripts/pm_hello.py").exists()
          and not (HOME / "extensions/skills/pm-greeter").exists())
    code, out = run("pm", "remove", "sample-persona")
    check("pm remove 人格包", code == 0 and not (HOME / "data/prompts/sample-persona.md").exists()
          and not (HOME / "personas/sample-persona-md").exists(), out[-120:])
    code, out = run("pm", "list")
    check("全卸后 list 为空", code == 0 and ("还没装" in out or "Nothing installed" in out), out)
    code, out = run("pm", "tap", "list")
    check("tap list 含官方 tap", code == 0 and "SHORiN-KiWATA/miyu-packages" in out, out)
    code, out = run("pm", "remove", "nope")
    check("卸不存在的包报错", code != 0)

    failed = [name for name, ok, _ in results if not ok]
    print(f"\n{len(results) - len(failed)}/{len(results)} PASS")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
