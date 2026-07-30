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
miyu 0.3.0
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
miyu web                     # 启动本地 WebUI（默认端口 4096）
miyu web --password          # 使用密码保护 WebUI
miyu powershell-init         # 安装或刷新 PowerShell 集成
miyu remove-shell-hook       # 删除 PowerShell 集成
```

启动 WebUI 后会自动打开浏览器。服务会监听本机网络接口；如果 Windows
防火墙允许局域网访问，建议使用 `miyu web --password` 设置访问密码。

## 四、从旧版本安全升级

升级只需要替换程序目录。模型配置、API Key、聊天状态和日志位于用户目录，
不在发布压缩包内。请不要删除或覆盖 `%APPDATA%\miyu`。

### 1. 找到当前安装目录

在可以正常运行 `miyu` 的 Windows PowerShell 中执行：

```powershell
Get-Command miyu -All | Format-List CommandType,Source,Definition
```

如果 `Source` 为空，请查看 `Definition`。其中 `miyu.exe` 所在的文件夹就是
当前安装目录，例如 `C:\Tools\Miyu`。

### 2. 备份个人配置

以下命令会创建一个带时间戳的完整配置备份：

```powershell
$config = Join-Path $env:APPDATA "miyu"

if (Test-Path $config) {
    $backup = Join-Path $env:APPDATA (
        "miyu-backup-" + (Get-Date -Format "yyyyMMdd-HHmmss")
    )
    Copy-Item -LiteralPath $config -Destination $backup -Recurse
    Write-Host "配置已备份到：$backup"
}
```

备份中可能包含 API Key，不要上传或分享这个文件夹。

### 3. 关闭旧版程序

关闭正在运行的 Miyu、`miyu web` 和使用 Miyu 的旧 PowerShell 窗口。可以用
下面的命令确认是否仍有进程：

```powershell
Get-Process miyu -ErrorAction SilentlyContinue
```

如果确认这些进程都可以结束，再运行：

```powershell
Get-Process miyu -ErrorAction SilentlyContinue | Stop-Process
```

### 4. 覆盖程序文件

1. 将新版 `Miyu-windows-x86_64.zip` 解压到一个临时文件夹。
2. 把临时文件夹内的全部内容复制到原安装目录。
3. Windows 询问时选择替换同名文件。

必须一起更新 `miyu.exe`、`miyu.cmd`、`rg.exe`、`share` 文件夹和说明文件，
不要只复制 `miyu.exe`。也不要把新版解压到旧目录的子文件夹中。

### 5. 刷新 PowerShell 集成

在安装目录中运行：

```powershell
.\miyu.exe powershell-init
```

关闭所有旧 PowerShell 窗口并重新打开，然后验证：

```powershell
miyu --version
miyu config validate
```

当前发布包应显示 `miyu 0.3.0`。原来的模型配置和 API Key 应继续有效。如需
验证 WebUI，可运行 `miyu web`。

### 6. 升级失败时回退

升级前也可以把整个旧程序目录复制一份作为程序备份。如果新版无法启动，先
结束 `miyu.exe`，再用旧程序备份覆盖安装目录。只有在配置也发生异常时，才
需要用步骤 2 创建的备份恢复 `%APPDATA%\miyu`。

## 五、常见问题

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

### 编辑自定义人格或用户身份的“内容”

在“内容”一栏按 Enter 会打开 Miyu 内置的多行提示词编辑器，不再依赖系统记事本：

- 直接输入或粘贴提示词，Enter 换行，Tab 插入 4 个空格。
- 按 `Ctrl+S` 保存。对于用户身份等普通文本字段，会同时返回并保存当前表单。
- 按一次 Esc 会提示存在未保存修改，再按一次 Esc 才会放弃修改。
- 方向键、Home、End、PageUp 和 PageDown 可移动光标。

Windows 11 新版记事本采用单实例标签页机制，启动进程可能在文件真正保存前就
退出，容易造成“另存为”或 Miyu 读取不到新内容。因此本版本不会把记事本作为
默认编辑器。

也可以通过 `VISUAL` 或 `EDITOR` 指定其他编辑器。例如，临时使用 VS Code：

```powershell
$env:EDITOR = "code.cmd --wait"
miyu config
```

如需为当前用户永久设置，请运行：

```powershell
[Environment]::SetEnvironmentVariable(
    "EDITOR",
    "code.cmd --wait",
    "User"
)
```

设置后关闭并重新打开 PowerShell。外部编辑器命令必须等待文件关闭后再退出，
因此 VS Code 需要保留 `--wait` 参数。如果外部编辑器无法启动，Miyu 会显示
错误并安全回退到内置编辑器。即使把 `EDITOR` 设置为 `notepad.exe`，Windows
版本也会改用内置编辑器，避免上述异步保存问题。

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

## 六、取消 PowerShell 集成

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

## 七、分享与安全提示

- 只需分享 `Miyu-windows-x86_64.zip`，不要分享自己的配置目录。
- 不要把 API Key 写进压缩包、截图或聊天消息。
- 接收者需要使用自己的模型服务账号和 API Key。
- 收到更新版本后，请按照“从旧版本安全升级”一节先备份配置，再覆盖完整
  程序目录并运行 `miyu powershell-init`。
