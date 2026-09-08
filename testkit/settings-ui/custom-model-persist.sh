#!/bin/bash
# 自定义模型的落盘往返：加两个 → 取消激活其中一个 → 保存退出 → 重进看还在不在。
# 用法: MIYU_BIN=<二进制> custom-model-persist.sh <outdir>
OUT="${1:-/tmp/custom-model-persist}"
HOME_DIR="$OUT/home"
mkdir -p "$OUT" "$HOME_DIR"
BIN="${MIYU_BIN:-$(dirname "$0")/../../target/debug/miyu}"
S=miyucp$$
run_session() {
  tmux kill-session -t "$S" 2>/dev/null
  tmux new-session -d -s "$S" -x 110 -y 40 "env MIYU_HOME=$HOME_DIR XDG_RUNTIME_DIR=$HOME_DIR/xdg-run MIYU_LANG=zh_CN.UTF-8 TERM=xterm-256color $BIN config"
  sleep 2
}
cap() { sleep 0.6; tmux capture-pane -p -t "$S" > "$OUT/$1.txt"; echo "captured $1"; }
key() { tmux send-keys -t "$S" "$@"; sleep 0.4; }
type_text() { tmux send-keys -t "$S" -l "$1"; sleep 0.4; }
add_model() { key n; type_text "$1"; key Enter; key s; sleep 1; }

run_session
key Enter          # 供应商和模型
sleep 3
add_model "beta-keep"
add_model "beta-inactive"
key Tab            # 光标停在刚加的 beta-inactive 上，取消激活
cap 01-before-save
key q              # 回主菜单
key q              # 退出 → 问是否保存
key Enter          # 选中「保存」
sleep 1.5
tmux kill-session -t "$S" 2>/dev/null

echo "===== 落盘的 providers[0] ====="
python3 - "$HOME_DIR" <<'PY'
import json, sys, pathlib
path = pathlib.Path(sys.argv[1]) / "config" / "config.json"
data = json.loads(path.read_text())
provider = data["providers"][0]
print("id             :", provider["id"])
print("models         :", provider.get("models"))
print("custom_models  :", provider.get("custom_models"))
PY

run_session
key Enter
sleep 3
cap 02-after-reload
tmux kill-session -t "$S" 2>/dev/null
echo "shots in $OUT"
