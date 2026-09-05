# Miyu 脚本工具接口

一个脚本工具 = 一个可执行文件。Miyu 从文件开头的注释里读工具契约，用 JSON 传参把它跑起来，再把 stdout 交给模型。本文是编写者（人或 AI）的接口说明；模型侧的同一份契约在内置技能 `script-creator`（`src/skills/script-creator.md`）里。

## 目录与优先级

| 层 | 目录 | 说明 |
|---|---|---|
| 内置 | `/usr/share/miyu/scripts/personas/default/` | 随包安装，只有默认人格看得到 |
| 全局 | `~/.miyu/data/scripts/` | 所有人格可见 |
| 人格 | `~/.miyu/data/scripts/personas/<人格>/` | 只有该人格可见，`manage_script` 默认落在这里 |

同名（同 id）后者覆盖前者。每层目录下可有一个 `index.json`，它是**覆盖层**：条目里显式写了的字段压住脚本头部，没写的从头部补；`disabled` 名单能屏蔽本层和更低层的同名脚本（内置脚本只能这样屏蔽）。

扫描只看各目录的顶层文件。目录内容一变（新增/修改/删除文件），下一个工具回合自动重扫，不用重启。

## 脚本头部

紧跟 shebang 的注释行，`# 键: 值` 格式（`//` 注释也认）。不认识的注释行（coding 声明、许可证）会被跳过，第一个非注释行结束头部。

```
#!/usr/bin/env python3
# Display name: 番组日历
# Description: Query the Bangumi airing calendar. Use for "what airs today" questions.
# Timeout: 60
# Group: research
# Argv: none
# Parameters:
# {
#   "type": "object",
#   "properties": {
#     "action": {"type": "string", "enum": ["calendar", "search"], "description": "calendar (default) or search"},
#     "query": {"type": "string", "description": "search keyword"}
#   }
# }
```

| 键 | 别名 | 说明 |
|---|---|---|
| `Description` | `描述`、`功能介绍` | 模型看的描述。**英文**，首句 ≤60 字符（stub 加载模式下只显示首句）。先说做什么、何时用，再说注意事项 |
| `Display name` | `显示名称`、`工具名称` | 人看的名字，可中文 |
| `Id` | | 工具名，`^[a-zA-Z][a-zA-Z0-9_]*$`。不写则用文件名 stem，非字母数字折成 `_`（`battery-care.py` → `battery_care`） |
| `Parameters` | `params`、`schema`、`参数` | JSON Schema 对象，可单行或多行块；每个属性写 `description`。不写=接受任意 JSON 对象 |
| `Timeout` | `timeout_seconds`、`超时` | 秒，默认 120，上限 300 |
| `Group` | `groups`、`分组` | `load_tools` 分组，逗号分隔 |
| `Argv` | | `none`（默认）或 `flags`，见下 |

## 运行时契约

- **传参**：全部参数作为一个 JSON 对象写进 stdin；同一份 JSON 在 64KB 以内时也放在环境变量 `MIYU_ARGS_JSON`。
- **argv**：默认不传。`Argv: flags` 时额外展开为 `--key=value`：字符串/数字 `--query=x --limit=5`，`true` 只给 `--json`，`false`/`null` 省略，数组和对象给紧凑 JSON 字符串。键按字典序。
- **无 schema 时**：工具接受任意对象；特殊键 `stdin`（字符串）会替换 stdin 里的 JSON，原文透传。
- **输出**：结果打到 stdout；退出码 0 成功。失败时非零退出并输出 `{"ok":false,"error":"…","fix":"用户该做什么"}`。Miyu 回给模型的是 `{success, exit_code, stdout, stderr}`。
- **上限**：单流 8MiB 硬截断，展示 20000 字符软截断。列表类结果要给 `limit` 参数。
- **缓存目录**：`MIYU_SCRIPT_CACHE_DIR` 指向 Miyu 的缓存目录，登录态、cookie、中间产物放这里；变量不存在（终端直接跑）时退回 XDG 默认。
- **输出格式**：默认紧凑可读，提供 `format=json` 供逐字段处理。

## 用 manage_script 注册

| 动作 | 说明 |
|---|---|
| `register` | `path` 给任意绝对路径 → 复制进目标层（默认 persona，`scope=global` 可选）并设可执行位；文件已在某用户层里则就地注册。`id` 可省（头部 `Id:` > 文件名）。`description/parameters/timeout_seconds/display_name` 只在要覆盖头部时传。合并更新：只给的字段才改。同名文件已存在要 `overwrite=true` |
| `unregister` | 按 id 找用户层条目或自动检测文件，写进 `disabled`；`delete_file=true` 连文件删掉。内置脚本只能屏蔽（记在 persona 层），之后 `register` 只给 `id` 即可恢复 |
| `list` | 已注册（含所在层）/未注册（缺描述的文件）/disabled |

注册前会校验：有 shebang、能凑出描述（头部或参数）、schema 是 `type: object`。

## 骨架

Python（stdin JSON）：

```python
#!/usr/bin/env python3
# Display name: Example
# Description: One sentence under 60 characters. Then when to use it.
# Parameters: {"type":"object","properties":{"query":{"type":"string","description":"what to look up"}},"required":["query"]}
import json, os, sys

def fail(error, fix=None, code=2):
    print(json.dumps({"ok": False, "error": error, "fix": fix}, ensure_ascii=False))
    sys.exit(code)

def main():
    raw = os.environ.get("MIYU_ARGS_JSON") or sys.stdin.read() or "{}"
    args = json.loads(raw)
    query = args.get("query") or fail("query is required")
    print(f"result for {query}")

if __name__ == "__main__":
    main()
```

Bash（argv flags）：

```bash
#!/usr/bin/env bash
# Display name: Example
# Description: One sentence under 60 characters.
# Argv: flags
# Parameters: {"type":"object","properties":{"query":{"type":"string","description":"what to look up"}},"required":["query"]}
set -euo pipefail
query=""
for arg in "$@"; do case "$arg" in --query=*) query="${arg#*=}";; esac; done
[ -n "$query" ] || { echo '{"ok":false,"error":"query is required"}'; exit 2; }
echo "result for $query"
```

## 常见问题

- **注册了但工具没出现**：`manage_script action=list` 看它在不在 `unregistered`（缺描述）或 `disabled` 里；id 是否与内建工具重名。
- **脚本跑起来报 Exec format error**：没有 shebang。
- **描述在 stub 模式下被截断**：首句太长，把第一句压到 60 字符以内。
- **同一文件想改 id**：加 `# Id:` 头部，或 register 时传 `id`。
