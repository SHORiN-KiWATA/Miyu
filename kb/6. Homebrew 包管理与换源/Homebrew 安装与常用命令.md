# Homebrew 安装与常用命令

macOS 上最主流的包管理器是 Homebrew，用它可以安装命令行工具和 GUI 应用。

## 安装

```zsh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

安装完成后按提示把 Homebrew 加入 PATH（Apple Silicon）：

```zsh
echo 'eval "$(/opt/homebrew/bin/brew shellenv)"' >> ~/.zprofile
eval "$(/opt/homebrew/bin/brew shellenv)"
```

## 常用命令

| 操作 | 命令 |
|---|---|
| 搜索软件 | `brew search 关键词` |
| 查看信息 | `brew info 包名` |
| 安装命令行工具 | `brew install 包名` |
| 安装 GUI 应用 | `brew install --cask 包名` |
| 更新 Homebrew 本体 | `brew update` |
| 升级所有软件 | `brew upgrade` |
| 卸载 | `brew uninstall 包名` |
| 查看已装 | `brew list` |
| 检查问题 | `brew doctor` |
| 清理旧版本和缓存 | `brew cleanup` |
| 管理后台服务 | `brew services list / start / stop 服务名` |

## 注意事项

- Apple Silicon 的 Homebrew 装在 `/opt/homebrew`，Intel 装在 `/usr/local`。
- 安装大体积软件前先 `brew info` 看依赖数量，避免装出一堆用不上的依赖。
- 官方源在国内可能很慢，见「Homebrew 换源提速」。
