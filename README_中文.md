# Miyu Windows 中文使用说明

Miyu 是一个可以在终端中使用的 AI 助手。本压缩包为 Windows 10/11
64 位版本，无需安装 Rust 或编译源代码。

## 一、开始使用

### 1. 解压程序

将压缩包完整解压到一个长期使用的位置，例如：

```text
C:\Tools\Miyu
```

不要直接在压缩包内运行程序。完成 PowerShell 集成后也不要随意移动或
删除该目录，否则需要在新位置重新执行集成命令。

### 2. 打开 Windows PowerShell

进入解压后的目录，在文件夹空白处按住 Shift 并单击鼠标右键，选择
“在此处打开 PowerShell 窗口”。也可以在资源管理器地址栏输入
`powershell` 后按 Enter。

先确认程序可以运行：

```powershell
.\miyu.exe --version
```

正常情况下会显示：

```text
miyu 0.2.1
```

### 3. 首次配置

运行初始化向导：

```powershell
.\miyu.exe init
```

按照界面提示配置模型服务、模型名称和 API Key。每位使用者都应填写
自己的 API Key，不要共享其他人的密钥或 `%APPDATA%\miyu` 配置目录。

如需稍后修改配置，可运行：

```powershell
.\miyu.exe config
```

### 4. 启动 Miyu

在解压目录运行：

```powershell
.\miyu.exe
```

## 二、集成到 Windows PowerShell

在解压目录运行：

```powershell
.\miyu.exe powershell-init
```

然后关闭所有旧的 Windows PowerShell 窗口，再重新打开一个窗口。此后可在
任意目录直接启动 Miyu：

```powershell
miyu
```

也可以在 PowerShell 提示符中直接输入自然语言，例如：

```text
帮我分析一下当前目录中的项目
把这里的 Markdown 文件列出来
解释这个报错可能是什么原因
```

正常的 PowerShell 命令仍会由 PowerShell 执行，例如：

```powershell
Get-ChildItem
Get-Process
cd C:\Tools
.\程序.exe --help
```

集成功能使用当前用户的 PowerShell Profile，不会替换 PowerShell。若原来的
Profile 已经存在，Miyu 会在首次集成时创建一个 `.miyu-backup` 备份。

## 三、常用命令

```powershell
miyu                         # 启动交互式 AI 助手
miyu --version               # 查看版本
miyu init                    # 运行首次初始化
miyu config                  # 修改配置
miyu paths                   # 查看配置、数据和日志路径
miyu powershell-init         # 安装或刷新 PowerShell 集成
miyu remove-shell-hook       # 删除 PowerShell 集成
```

## 四、常见问题

### Windows 提示“未知发布者”或阻止运行

当前程序没有商业代码签名。请确认压缩包来自可信发送者。若 Windows
SmartScreen 弹出提示，可选择“更多信息”，核对文件后选择“仍要运行”。

也可以右键单击压缩包或 `miyu.exe`，打开“属性”，勾选“解除锁定”后应用。

### 提示无法运行 PowerShell Profile 或脚本被禁止

仅在确认文件来源可信时，为当前用户允许本地脚本：

```powershell
Set-ExecutionPolicy -Scope CurrentUser RemoteSigned
```

确认后关闭并重新打开 Windows PowerShell。该命令只修改当前用户的执行策略，
但仍属于安全设置变更，不要对来源不明的脚本使用更宽松的策略。

### 输入 `miyu` 后提示找不到命令

先回到解压目录重新执行：

```powershell
.\miyu.exe powershell-init
```

随后关闭并重新打开 Windows PowerShell。如果程序目录曾被移动，也需要再次
执行这条命令。

### 自然语言请求无法得到回答

运行 `miyu config` 检查接口地址、模型名称和 API Key，并确认计算机可以访问
所配置的模型服务。API Key 无效、余额不足或网络受限也会导致请求失败。

### 文件搜索功能提示找不到 `rg`

部分 `glob` 和 `grep` 文件工具依赖 ripgrep（`rg.exe`）。通过
`build-windows.ps1` 生成的完整压缩包已经包含它。若此处仍提示找不到 `rg`，
请确认解压时保留了同目录下的 `rg.exe`，不要只复制 `miyu.exe`。

### 在哪里查看日志和配置

运行：

```powershell
miyu paths
```

通常配置位于 `%APPDATA%\miyu`，日志及缓存位于
`%LOCALAPPDATA%\miyu`。

## 五、取消 PowerShell 集成

运行：

```powershell
miyu remove-shell-hook
```

如果 `miyu` 命令已经不可用，也可以进入程序目录运行：

```powershell
.\miyu.exe remove-shell-hook
```

关闭并重新打开 Windows PowerShell 后完成卸载。该操作只删除 Miyu 添加的
集成区块和生成的 Hook，不会删除其他 PowerShell Profile 内容。

## 六、分享与安全提示

- 只需分享 `Miyu-windows-x86_64.zip`，不要分享自己的配置目录。
- 不要把 API Key 写进压缩包、截图或聊天消息。
- 接收者需要使用自己的模型服务账号和 API Key。
- 收到更新版本后，可解压到固定目录覆盖旧程序，再运行一次
  `miyu powershell-init` 刷新集成路径。
