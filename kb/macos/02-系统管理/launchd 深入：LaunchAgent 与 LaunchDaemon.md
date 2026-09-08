# launchd 深入：LaunchAgent 与 LaunchDaemon

macOS 的开机自启与后台服务都由 launchd 统一管理。这份是「开机自启与 LaunchAgent」的进阶篇。

## 两种任务

| 类型 | 位置 | 运行环境 | 用途 |
|---|---|---|---|
| LaunchAgent | `~/Library/LaunchAgents`、`/Library/LaunchAgents` | 用户登录后，当前用户会话 | 用户级应用、代理、输入法等 |
| LaunchDaemon | `/Library/LaunchDaemons`、`/System/Library/LaunchDaemons` | 系统启动即运行，无用户会话 | 系统级服务（需 sudo） |

系统自带的在 `/System/Library/LaunchAgents` 与 `/System/Library/LaunchDaemons`，别去动。

## plist 关键键

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.mydaemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/mytool</string>
        <string>--flag</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/mytool.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/mytool.err.log</string>
</dict>
</plist>
```

- `Label`：唯一标识，习惯用反向域名，加载时按它找文件。
- `ProgramArguments`：可执行文件路径 + 参数。**不要用 `Program` 单键**（有坑）。
- `RunAtLoad`：加载即启动。
- `KeepAlive`：`true` 表示挂了自动拉起；也可以写 dict 做条件（如网络变化时）。
- `StartInterval`：固定间隔运行（秒）；`StartCalendarInterval`：定时（如每天 3 点）。
- 日志：不配 `StandardOutPath/Error` 时输出进系统日志，排障难受。

## 常用命令

```zsh
launchctl load ~/Library/LaunchAgents/com.example.plist    # 加载
launchctl unload ~/Library/LaunchAgents/com.example.plist  # 卸载
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.example.plist  # 新式加载
launchctl bootout gui/$(id -u)/com.example                 # 新式卸载
launchctl start com.example    # 手动启动一次
launchctl stop com.example     # 手动停止
launchctl list | grep example  # 查看状态（PID/退出码）
launchctl print gui/$(id -u)/com.example  # 详细状态
```

新系统推荐 `bootstrap/bootout` 替代 `load/unload`（后者已废弃）。

## 排障

1. 先看 `launchctl list` 里 PID 和 LastExitStatus。
2. 退出码 78 常见于 plist 语法错（`plutil -lint` 检查）。
3. 改完 plist 要 `bootout` 再 `bootstrap`，只 `start` 不会重读配置。
4. GUI 程序当 Daemon 跑没界面是正常的；要界面就用 Agent。

## 常见坑

- plist 权限：Daemon 需 root 所有、644 权限。
- 路径写错：ProgramArguments 里的路径必须绝对路径。
- `RunAtLoad` 加 `KeepAlive` 都开，程序自己反复崩溃会被 launchd 拉黑（限制拉起次数）。
- 用 `sudo launchctl` 操作 Daemon，`launchctl` 默认只操作自己用户的 Agent。
