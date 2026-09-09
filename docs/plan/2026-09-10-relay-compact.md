# 中转线统一走 Miyu 压缩（2026-09-10）

用户裁定：claude-code / codex / antigravity 三条中转线**统一用 Miyu 自己的压缩**，不依赖 CLI 那头的自压缩。理由是统一、可控。

基线提交：`1a8bf6e3`。

## 一、根因（09-10 取证）

### 1. 中转轮从不进 footprint，也不进回灌候选

三条线的工具事件（原生工具与 `mcp__miyu__*` 桥工具都一样）翻成 `RemoteToolStarted/Finished`，
由 `turn_loop::record_remote_tool_chunk` 折成 `remote: true` 的 `ToolFlowRound`。两个采集器都看不见它：

| 采集器 | 位置 | 为什么漏 |
|---|---|---|
| footprint（`<read-files>/<modified-files>`） | `turn_loop/mod.rs` 本地工具执行分支 | 只有本地执行的工具调 `tool_call_footprint`；remote 轮走 `append_remote_tool_flow` |
| 回灌候选 `touched_files` | `tool_report.rs::replay_rounds` | 第一行 `filter(!round.remote)` |

活库证据（`~/.miyu/state/conversation.db` 副本）：42 个 remote 轮（claude-code 21、antigravity 21）footprint 全空；
antigravity 那 21 轮里有 `view_file`×4、`write_to_file`×4，真读真写了也是空。
bda27508 修的 `edit`/`patchText` 只救了直连线。

### 2. claude-code 线上 Miyu 与 CLI 两套压缩抢跑

`builder.rs` 把 Miyu 有效窗口透传成 `--autocompact <window>`。两条触发线量的是同一个数（最近一次 API usage）：

| 窗口 W | Miyu 0.8×W | claude W−20k−13k | 谁先 |
|---|---|---|---|
| 100k | 80,000 | 67,000 | claude |
| 168k（默认） | 134,400 | 135,000 | Miyu，早 600 tok |
| 200k | 160,000 | 167,000 | Miyu |

默认下 Miyu 先到；一压，哈希链断、CLI 会话重开全量重放，claude 的 autocompact 从没跑过。
压缩请求 scope=compact 在中转线上恒 ephemeral 不续传（`cli_relay/mod.rs`），所以 fork 在这条线上结构上不存在——
这就是 09-09 遗留「claude-code 线 fork 未交付」的原因，不是待复现的 bug。

codex 有 `model_auto_compact_token_limit`/`model_context_window` 键但 Miyu 没传；agy 有内部压缩无外部旋钮。

## 二、改动

### P1 中转轮进 footprint

`record_remote_tool_chunk` 在 `RemoteToolFinished` 且 `ok` 时，用 Started 时存下的 name+arguments 算
`tool_call_footprint`，返回给调用方合进 `turns.tool_footprint`（与本地工具"成功才记"同一口径）。

`tool_call_paths` 认三线原生名：

| 线 | 名字 | 路径字段 | 读/写 |
|---|---|---|---|
| claude | Read | `file_path` | 读 |
| claude | Edit / Write / MultiEdit | `file_path` | 写 |
| claude | NotebookEdit | `notebook_path` | 写 |
| agy | view_file | `path`（`normalize_native_arguments` 已把 AbsolutePath 映成 path） | 读 |
| agy | write_to_file / replace_file_content / multi_replace_file_content | `path` | 写 |
| codex | edit（`file_change` 折出来的） | `paths` 数组 | 写 |

### P2 回灌候选补上 remote 轮

`replay_rounds` 的 remote 过滤是回放契约的一部分，不动。`build_compact_extras` 多收两份落库 footprint
（折叠区、尾巴），候选 = `touched_files(fold)` 按近因排序 + 折叠区 footprint 里剩下的路径；
跳过集 = 尾巴回放里读过的 + 尾巴 footprint 的 read。直连线的路径两边都会出现，按解析后路径去重。

### P3 claude-code 不再传 `--autocompact`

用户裁定「别固定上限，别设置就行了」。删 `ClaudeCodeRuntime.autocompact` 整条链。
codex 不加 `model_auto_compact_token_limit` 覆盖。

后果如实记录：CLI 那头仍有各自默认的自压缩（claude 按模型真实窗口 200k/1M，codex 按模型默认），
只要 Miyu 的 `context_window` 配置不大于 CLI 的真实窗口，Miyu 的 0.8 线永远先到。

## 三、验证

- §5.1 先证红：`tool_footprint_recognizes_relay_native_tools` 在改前跑红（Edit/Read/view_file 全部 None）。
- 单测（全绿）：三线名字解析；`record_remote_tool_chunk` Finished ok 出 footprint、失败/未知工具不出；
  remote-only 折叠区无 footprint 时回灌为空、有 footprint 时回灌 edited.rs 且跳过尾巴读过的 seen.rs；
  claude 参数不含 `--autocompact`。
- `test_scripts/refactor-check.sh` 五道门禁。
- 真机（`testkit/relay-compact/relay_probe.py`，跑完打 PASS/FAIL）：隔离 MIYU_HOME + 真 claude-code(haiku)，dev 会话 Read+Edit、Write 各一轮
  → `miyu compact` → 查 `turns.tool_footprint`、摘要尾 `<read-files>/<modified-files>`、`compact_extras.restored`。

### 真机结果（09-10 01:3x，隔离 home，claude-code/haiku，dev 模式）

四轮会话：① Read notes.txt + Edit（beta→gamma）② Write hello.txt ③④ 闲聊。`compact_tail_tokens=1` 让 ③④ 留尾巴、①② 进折叠区
（默认 16384 的尾巴预算会把四轮小会话整个留住，报「没有可压缩的内容」——这是测法问题不是 bug）。

| 观察点 | 改前（09-10 活库） | 改后 |
|---|---|---|
| `turns.tool_footprint`（seq1） | 空 | `read=[notes.txt] modified=[notes.txt]` |
| `turns.tool_footprint`（seq2） | 空 | `modified=[hello.txt]` |
| 摘要尾 `<read-files>` | 无 | notes.txt |
| 摘要尾 `<modified-files>` | 无 | hello.txt、notes.txt |
| `compact_extras.restored` | `[]` | hello.txt（1 行）、notes.txt（122 行，第 2 行已是 gamma） |
| claude 进程参数（单测 `relay_leaves_compaction_to_miyu` 抓 args.txt） | `--autocompact <窗口>` | 无 |

工具名在 tool_flow 里就是原生的 `Read`/`Edit`/`Write`，参数是 `file_path`，与单测形状一致。

## 四、改动清单

| 文件 | 改动 |
|---|---|
| `src/agent/tool_report.rs` | `path_arg` 认 `path`/`file_path`/`notebook_path`；新增 `paths_arg`；`tool_call_paths` 认三线原生名 |
| `src/agent/turn_loop/mod.rs` | `record_remote_tool_chunk` 返回 footprint 增量（Finished 且 ok），两处调用点合进 `turns.tool_footprint` |
| `src/agent/compact_extras.rs` | 新 `FoldFootprints`；候选 = 回放视图 + 折叠区 footprint；跳过集加尾巴 footprint.read |
| `src/agent/compact.rs` | 折叠区 footprint 合并前快照；尾巴 footprint 另载；传入 `build_compact_extras` |
| `src/llm/openai_compatible/claude_code/mod.rs`、`builder.rs` | 删 `autocompact` 整条链 |
| `docs/compact-plan.md` | 09-10 案卷两行 |
| 测试 | `tests/artifacts.rs`、`tests/remote_tools.rs`（新）、`tests/compact_extras.rs`、`tests/claude_code.rs` |
