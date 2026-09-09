# shellhook 提问走查

复现「shellhook 触发 → 工具提问 → 什么都不做,几秒后回合被取消」。

## 形态

fish hook 把命令行管道喂进来:

```fish
printf '%s' "$buffer" | miyu --shell-intercept --shell fish --stdin
```

stdin 是**管道**、stdout 是终端、进程持有控制终端(`/dev/tty`)。`run.py` 用
`pty.openpty()` + `TIOCSCTTY` 复刻这一组 fd,桩模型立刻发出一个 `ask_question`
工具调用。

## 根因(09-10)

`spawn_hangup_watchdog` 的 `terminal_hangup()` 裸 poll **stdin** 判挂断。管道
写端(`printf`)一退出,stdin 就常驻 `POLLHUP`;而 `question_tui::ask` 的
`QuestionSession::start` 正是看门狗的**第一个调用点**——面板一打开就按下
「500ms 探测 + 5 秒宽限」的倒计时,到点 `std::process::exit(1)`。daemon 那边
`IpcRunGuard` 认这个客户端是一次性的(`origin_tty` 存在),断线即取消 run,
于是回合以 `interrupted` 落库,用户看到面板自己消失。

REPL 不受影响:REPL 的 stdin 就是终端,而且它的 IPC 连接不带 `origin_tty`。

修法见 `src/cli/mod.rs`:`hangup_watch_fd` —— stdin 是终端才盯 stdin,否则盯
控制终端;拿不到控制终端就永不判挂断。

## 跑法

```sh
cargo build                                   # 静态资源与 CLI 都编进二进制
python3 testkit/shellhook-question/run.py             # 不按键,看面板能不能活过 5.5s
KEYS=arrows python3 testkit/shellhook-question/run.py # 3s 后敲 ↑ ↓
KEYS=answer python3 testkit/shellhook-question/run.py # 再敲回车提交
MIYU_LOG=debug python3 testkit/shellhook-question/run.py   # 同时留 daemon 日志
```

产物在 `~/.cache/miyu-shellhook-question/`:`raw.bin`(终端原始输出)、
`chunks.json`(带时间戳的分块)、`report.json`、`daemon.log`。daemon 的详细
日志在 `$MIYU_HOME/cache/logs/miyu.*.log`。

## 判定

| | 修前 | 修后 |
|---|---|---|
| 面板文字 | 出现 | 出现 |
| 客户端进程 | ~5.5s 后 `exit(1)` | 一直活着 |
| daemon 日志 | `one-shot client disconnected; its run was cancelled` | 无 |
| 回合状态 | `interrupted` | `completed`,答案落进 `question_exchanges` |

`report.json` 的 `panel_seen` / `cancel_seen` / `exited_at` / `returncode` 四个
字段足够判断。
