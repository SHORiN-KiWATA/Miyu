# Miyu for Windows

This port runs Miyu as a native Windows console application. It keeps the TUI,
chat, configuration, knowledge base, memory, web tools, image generation,
Windows clipboard access, alarms, file tools, and custom script tools.

## Requirements

- Windows 10 or Windows 11 (64-bit)
- A terminal with UTF-8 and ANSI support; Windows Terminal is recommended
- An OpenAI-compatible model endpoint configured in Miyu
- `rg.exe` (ripgrep) for the `glob` and `grep` tools; packages produced by
  `build-windows.ps1` include it
- Optional: Git for updating the default knowledge base

## Run the packaged build

Open PowerShell in the `dist` directory:

```powershell
.\miyu.exe --version
.\miyu.exe init
.\miyu.exe config
.\miyu.exe
.\miyu.exe web
```

`miyu.cmd` is provided for Command Prompt users.

The WebUI uses port 4096 by default and opens the browser automatically. It
listens on the machine's network interfaces, so use `miyu web --password` when
Windows Firewall permits access from the local network.

## Integrate with Windows PowerShell

From the `dist` directory, install Miyu into the current user's built-in
Windows PowerShell profile:

```powershell
.\miyu.exe powershell-init
```

Close and reopen Windows PowerShell. You can then run `miyu` from any directory,
or type a natural-language request directly at the PowerShell prompt. Existing
PowerShell commands continue to run normally. The integration uses PSReadLine
and does not replace PowerShell itself.

The generated hook is stored at
`%APPDATA%\miyu\shell\powershell-hook.ps1`. The installer adds one marked block
to `Documents\WindowsPowerShell\Microsoft.PowerShell_profile.ps1`; if that
profile already exists, its original version is copied once to a
`.miyu-backup` file.

To remove the integration:

```powershell
.\miyu.exe remove-shell-hook
```

Miyu stores per-user files in the normal Windows application directories:

- configuration: `%APPDATA%\miyu`
- data/state: `%APPDATA%\miyu`
- cache/logs: `%LOCALAPPDATA%\miyu`
- generated images: the user's Pictures folder under `miyu`

Run `.\miyu.exe paths` to print the exact paths on the current machine.

## Upgrade an existing Windows installation

Only replace the program directory. Configuration, API keys, conversation
state, and logs are stored in the user's application directories and are not
included in the release archive. Do not delete or overwrite `%APPDATA%\miyu`.

First, locate the executable used by the PowerShell integration:

```powershell
Get-Command miyu -All | Format-List CommandType,Source,Definition
```

When `Source` is empty, read the executable path from `Definition`. Back up the
per-user configuration before continuing:

```powershell
$config = Join-Path $env:APPDATA "miyu"

if (Test-Path $config) {
    $backup = Join-Path $env:APPDATA (
        "miyu-backup-" + (Get-Date -Format "yyyyMMdd-HHmmss")
    )
    Copy-Item -LiteralPath $config -Destination $backup -Recurse
    Write-Host "Configuration backup: $backup"
}
```

The backup may contain API keys. Never upload or share it.

Close Miyu, `miyu web`, and old PowerShell windows. Check for remaining
processes with:

```powershell
Get-Process miyu -ErrorAction SilentlyContinue
```

Extract the new `Miyu-windows-x86_64.zip` into a temporary directory. Copy all
of its contents over the existing installation and replace files when asked.
Update `miyu.exe`, `miyu.cmd`, `rg.exe`, the `share` directory, and the
documentation together; copying only `miyu.exe` can leave incompatible runtime
files behind.

From the installation directory, refresh the PowerShell hook:

```powershell
.\miyu.exe powershell-init
```

Close and reopen PowerShell, then verify the upgrade:

```powershell
miyu --version
miyu config validate
```

The current package should report `miyu 0.3.0`. If the upgrade fails, stop
`miyu.exe` and restore a copy of the old program directory. Restore the
configuration backup only if the configuration itself was damaged.

## Build from source

Install Rust from <https://rustup.rs>. Use either the MSVC toolchain with the
Visual Studio C++ Build Tools or the GNU toolchain with MinGW-w64. Install
ripgrep if `rg --version` is not available:

```powershell
winget install --id BurntSushi.ripgrep.MSVC -e
```

Close and reopen PowerShell after installation, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1
```

The script runs the test suite, creates a release build, copies `rg.exe` and
runtime data, and writes the standalone package to `dist` plus
`Miyu-windows-x86_64.zip`.
With MinGW, it automatically uses an ASCII-only build cache when the source path
contains non-ASCII characters.

To build without running tests:

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1 -SkipTests
```

If crates.io is slow or unavailable in your network, add `-UseRsProxy`:

```powershell
powershell -ExecutionPolicy Bypass -File .\build-windows.ps1 -UseRsProxy
```

## Windows-specific behavior

- Clipboard text, copied files, and clipboard bitmap images use the native
  Windows clipboard API.
- AI shell commands run in non-interactive Windows PowerShell. The setting
  `skills.allow_command_execution=true` is still required for mutating commands.
- Custom `.ps1`, `.cmd`, `.bat`, `.py`, and `.exe` script tools are supported.
- Alarm workers run without opening a second console window and can be cancelled.
- Windows PowerShell integration is available through `powershell-init`.
- Multiline prompt and identity fields use Miyu's built-in Windows editor by
  default. It supports paste, newlines, cursor movement, `Ctrl+S` to save, and
  double-Esc to discard unsaved changes.
- `VISUAL` or `EDITOR` may select a waiting external editor such as
  `code.cmd --wait`. Windows Notepad is intentionally rejected because its
  single-instance tab handoff can return before the temporary file is saved.
- Linux-only diagnostic/AUR tools remain Linux-specific.

If PowerShell cannot find `miyu`, either run it as `.\miyu.exe` or add the
absolute `dist` directory to your user `Path` environment variable.
