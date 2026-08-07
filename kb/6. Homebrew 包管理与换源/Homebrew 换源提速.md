# Homebrew 换源提速

官方源在国内下载经常龟速，可以用清华 TUNA 镜像加速。

## 替换 Homebrew 本体源

```zsh
export HOMEBREW_BREW_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/brew.git"
export HOMEBREW_CORE_GIT_REMOTE="https://mirrors.tuna.tsinghua.edu.cn/git/homebrew/homebrew-core.git"
export HOMEBREW_API_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles/api"
export HOMEBREW_BOTTLE_DOMAIN="https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles"
brew update
```

## 持久生效

把上面四个 `export` 写进 `~/.zprofile`（或 `~/.zshrc`）即可长期生效。

## 恢复官方源

```zsh
export HOMEBREW_BREW_GIT_REMOTE="https://github.com/Homebrew/brew.git"
export HOMEBREW_CORE_GIT_REMOTE="https://github.com/Homebrew/homebrew-core.git"
unset HOMEBREW_API_DOMAIN HOMEBREW_BOTTLE_DOMAIN
brew update
```

## 其他镜像

- 中科大：`https://mirrors.ustc.edu.cn/homebrew-bottles`
- 阿里云：`https://mirrors.aliyun.com/homebrew/homebrew-bottles`

## 常见问题

- 换源后 `brew update` 报错：先删掉本地缓存 `rm -rf "$(brew --repo)/.git"` 再重新 update。
- 某些 cask（GUI 应用）走 CDN 下载仍慢，属正常现象，可挂代理或手动下载 dmg。
