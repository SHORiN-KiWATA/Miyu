# 终端与 Shell 高级

zsh 日常高级用法，提升效率。

## 别名与函数

```zsh
# ~/.zshrc 里加
alias ll='ls -lAhG'
alias ..='cd ..'
mcd() { mkdir -p "$1" && cd "$1"; }
```

`source ~/.zshrc` 生效。

## 历史记录

```zsh
history | tail -50      # 最近命令
!!                      # 上一条
!$                      # 上一条的最后一个参数
Ctrl+R                  # 反向搜索历史
```

## 管道与组合

```zsh
command | tee out.txt       # 输出到屏幕同时写文件
command > file 2>&1         # 输出和错误都进文件
command &> file             # 同上，简写
```

## 变量与展开

```zsh
echo $HOME                 # 家目录
echo ${PATH//:/\\n}        # 把 PATH 按 : 拆行
for f in *.md; do echo $f; done   # 遍历
```

## 任务控制

```zsh
Ctrl+Z      # 挂起当前任务
jobs        # 列出
bg %1       # 后台继续
fg %1       # 拉回前台
```

## 终端多标签

- Ghostty / iTerm2 / Terminal 都支持 Cmd+T 新标签、Cmd+D 分屏。
- tmux：`tmux new -s work` 创建会话，关终端重开 `tmux attach -t work` 恢复，远程神器。

## 快速跳转

```zsh
cd -                      # 回上一个目录
open .                    # Finder 打开当前目录
open -a "App"             # 用指定 App 打开
```

## 配置生效

改完 `.zshrc` 后 `source ~/.zshrc` 或重开终端。别把 export PATH 写重复，会越长越长。
