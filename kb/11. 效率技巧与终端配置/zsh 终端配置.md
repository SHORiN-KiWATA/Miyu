# zsh 终端配置

macOS 默认 shell 是 zsh，配置文件按加载时机区分。

## 配置文件

| 文件 | 加载时机 |
|---|---|
| `~/.zshenv` | 每次启动 zsh（含非交互） |
| `~/.zprofile` | 登录 shell 启动时 |
| `~/.zshrc` | 每次打开交互式终端 |
| `~/.zlogin` | 登录 shell 结束时 |
| `~/.zlogout` | 退出登录 shell 时 |

日常配置写 `~/.zshrc`；登录时初始化（如 Homebrew PATH）写 `~/.zprofile`。

## 常用配置示例

```zsh
# 别名
alias ll='ls -la'
alias gs='git status'

# 历史记录
HISTSIZE=5000
SAVEHIST=5000
setopt hist_ignore_dups

# 自动补全
autoload -Uz compinit && compinit

# 提示符（简单版）
PROMPT='%F{green}%n@%m%f %F{blue}%~%f %# '
```

改完配置立即生效：`source ~/.zshrc`。

## 环境变量

```zsh
export PATH="$HOME/bin:$PATH"        # 追加路径
export LANG=zh_CN.UTF-8              # 语言
```

## 常见框架与工具

- **oh-my-zsh**：`sh -c "$(curl -fsSL https://raw.githubusercontent.com/ohmyzsh/ohmyzsh/master/tools/install.sh)"`，主题/插件生态丰富。
- **zsh-autosuggestions**：命令行历史补全提示（oh-my-zsh 插件或独立安装）。
- **starship**：跨 shell 的现代化提示符。

## 排障

- **终端打开报错**：`zsh -x` 进入调试模式看哪行报错；最近改的配置先注释掉。
- **PATH 乱了**：`echo $PATH` 看顺序；`which <命令>` 确认用的哪个版本。
- **配置不生效**：确认写对了文件（`~/.zshrc` vs `~/.zprofile`），并 `source` 或重开终端。
