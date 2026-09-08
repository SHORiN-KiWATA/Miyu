# 开机自启与 LaunchAgent

macOS 开机自启分两种：登录项和 LaunchAgent。

## 登录项（图形界面）

系统设置 → 通用 → 登录项：

- **登录时打开**：添加/移除用户登录后自动启动的 App。
- **允许在后台**：管理后台代理类 App（如输入法、代理软件）。

## LaunchAgent（后台守护）

LaunchAgent 是 plist 描述的守护任务，按用户区分：

| 路径 | 作用 |
|---|---|
| `~/Library/LaunchAgents` | 当前用户登录后加载（常用） |
| `/Library/LaunchAgents` | 所有用户登录后加载 |
| `/Library/LaunchDaemons` | 系统级守护，开机即加载（需管理员） |

### 一个例子：开机自启某程序

`~/Library/LaunchAgents/dev.gqy.menubar.plist`：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.gqy.menubar</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>/Applications/顾清影.app</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
```

### 加载与卸载

```zsh
launchctl load ~/Library/LaunchAgents/xxx.plist     # 老语法
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/xxx.plist   # 新语法
launchctl bootout gui/$(id -u)/dev.xxx              # 按 Label 卸载
```

### 排障

- 查看已加载项：`launchctl list | grep xxx`
- plist 写错了不会报错提示，用 `plutil -lint xxx.plist` 检查语法。
- 修改 plist 后需要先 bootout 再 bootstrap 才会生效。
