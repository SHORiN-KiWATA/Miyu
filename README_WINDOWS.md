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
```

`miyu.cmd` is provided for Command Prompt users.

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
  Linux-only diagnostic/AUR tools remain Linux-specific.

If PowerShell cannot find `miyu`, either run it as `.\miyu.exe` or add the
absolute `dist` directory to your user `Path` environment variable.
