#!/bin/bash
# 自定义模型（n 添加 / Tab 激活 / Enter 编辑 / d 删除）的逐屏抓图。
# 用法: MIYU_BIN=<二进制> custom-model-shoot.sh <lang> <outdir> <home>
LANG_ARG="${1:-zh_CN.UTF-8}"
OUT="${2:-/tmp/custom-model-shots}"
HOME_DIR="${3:-$OUT/home}"
mkdir -p "$OUT" "$HOME_DIR"
BIN="${MIYU_BIN:-$(dirname "$0")/../../target/debug/miyu}"
S=miyucm$$
tmux kill-session -t "$S" 2>/dev/null
tmux new-session -d -s "$S" -x 110 -y 40 "env MIYU_HOME=$HOME_DIR XDG_RUNTIME_DIR=$HOME_DIR/xdg-run MIYU_LANG=$LANG_ARG TERM=xterm-256color $BIN config"
cap() { sleep 0.6; tmux capture-pane -p -t "$S" > "$OUT/$1.txt"; echo "captured $1"; }
key() { tmux send-keys -t "$S" "$@"; sleep 0.4; }
type_text() { tmux send-keys -t "$S" -l "$1"; sleep 0.4; }
sleep 2
cap 00-main
key Enter                       # 供应商和模型
sleep 3                         # 等第一次拉取失败
cap 01-browser
key n                           # 添加自定义模型
cap 02-add-form
type_text "beta-internal-x"
key Enter                       # 结束编辑
key s                           # 保存表单
sleep 1
cap 03-added
key Enter                       # 进编辑模型表单
cap 04-edit-form
key q                           # 退出编辑表单
sleep 0.5
cap 05-back
key Tab                         # 取消激活
cap 06-deactivated
key Tab                         # 再激活
cap 07-reactivated
key d                           # 删掉自定义模型
cap 08-deleted
key u                           # 撤销删除
cap 09-undo
key j                           # 移到下一行（若有拉取来的模型）
key d                           # 对非自定义行按 d 应给提示、不删供应商
cap 10-d-on-fetched
key q
sleep 0.5
cap 11-exit-prompt
key n                           # 不保存退出
sleep 0.5
tmux kill-session -t "$S" 2>/dev/null
echo "shots in $OUT"
