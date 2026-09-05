#!/bin/bash
# 用 tmux 驱动 miyu config TUI，逐屏 capture-pane 存文本。
# 用法: MIYU_BIN=<二进制> tui-shoot.sh <lang> <outdir>   （home 默认 testkit/settings-ui/home）
LANG_ARG="${1:-zh_CN.UTF-8}"
OUT="${2:-/tmp/tui-shots}"
mkdir -p "$OUT"
BIN="${MIYU_BIN:-$(dirname "$0")/../../target/debug/miyu}"
HOME_DIR="${MIYU_TEST_HOME:-$(dirname "$0")/home}"
S=miyucfg$$
tmux kill-session -t "$S" 2>/dev/null
tmux new-session -d -s "$S" -x 100 -y 40 "env MIYU_HOME=$HOME_DIR XDG_RUNTIME_DIR=$HOME_DIR/../xdg-run MIYU_LANG=$LANG_ARG TERM=xterm-256color $BIN config"
cap() { sleep 0.6; tmux capture-pane -p -t "$S" > "$OUT/$1.txt"; echo "captured $1"; }
key() { tmux send-keys -t "$S" "$@"; sleep 0.35; }
sleep 1.5
cap 00-main
# Embedding 模型菜单 (第 4 项)
key j; key j; key j; key Enter
cap 01-embedding
key Enter                 # 选中当前行（本地模型）→ 回主菜单
cap 01b-main-after-embedding
# 分级模型池 (第 5 项)
key j; key Enter
cap 02-tiers
key j; key j; key j; key j  # 跳过分隔行到 会话标题
key Enter
cap 03-aux-role-picker
key q
key j; key j
key Enter
cap 03b-deep-research-picker
key q
key q
# 接入通讯平台 (第 8 项) → QQ → 配置模型
key j; key j; key j; key Enter
cap 04-platforms
key Enter
cap 05-qq
key j; key Enter
cap 06-qq-models
key Enter
cap 07-qq-pool-picker
key q
key j; key j; key Enter
cap 08-qq-nonwhitelist-picker
key q; key q; key q; key q
cap 09-main-end
key q
sleep 0.5
tmux kill-session -t "$S" 2>/dev/null
