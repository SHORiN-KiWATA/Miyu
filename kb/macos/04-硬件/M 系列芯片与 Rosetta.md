# M 系列芯片与 Rosetta

Apple Silicon（M1/M2/M3/M4 等）Mac 与 Intel Mac 的软件生态差异。

## 怎么区分

- 系统设置 → 通用 → 关于本机，看「芯片」一栏。
- 命令行：`uname -m` 输出 `arm64` 是 Apple Silicon，`x86_64` 是 Intel。

## 软件兼容性

- **原生 arm64**：现在绝大多数主流软件（浏览器、Office、Adobe、微信等）都有 Apple Silicon 原生版。
- **x86_64 旧软件**：通过 Rosetta 2 转译运行，多数可用，个别性能略降。

## 安装 Rosetta 2

```zsh
softwareupdate --install-rosetta --agree-to-license
```

## 检查 App 架构

```zsh
file /Applications/xxx.app/Contents/MacOS/xxx
# 输出含 arm64 为原生，含 x86_64 为 Intel 版
```

## 用 x86_64 方式运行命令

```zsh
arch -x86_64 zsh            # 进入 x86_64 shell
arch -x86_64 brew install 包名   # 单独以 Intel 方式跑某命令
```

## Homebrew 与架构

- 单一架构原则：Apple Silicon 的 brew 装在 `/opt/homebrew`（arm64），Intel 的装在 `/usr/local`。
- 不要在同一台机器混装两套 brew，容易乱。
- 某些老包没有 arm64 版本时，`brew install` 会失败，属正常现象。

## 常见问题

- **Rosetta 提示需要安装但装不上**：确认系统版本 ≥ macOS 11.3，且存储空间足够。
- **x86_64 软件在 Apple Silicon 上崩溃**：升级软件到新版；Rosetta 无法解决时只能找替代品。
- **「此 Mac 无法运行 xxx」**：软件未适配且无 Rosetta 支持，只能换替代软件。
