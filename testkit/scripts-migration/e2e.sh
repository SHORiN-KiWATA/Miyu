#!/usr/bin/env bash
# 十件迁成内置脚本的工具,经真实二进制的工具注册表逐件调一遍(09-10 阶段 3)。
#
#   BIN=<miyu 二进制> bash testkit/scripts-migration/e2e.sh
#
# 隔离 MIYU_HOME;MIYU_SYSTEM_SCRIPTS_DIR 指向仓库里的 src/scripts,不吃已安装的
# /usr/share/miyu/scripts。`miyu tool <名字> <JSON>` 在本地装配 normal 注册表并
# 调用,不需要 daemon。离线可判的三件断言输出;联网的只看能否调通、有无 ok:false。
set -u
BIN=${BIN:-target/release/miyu}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${OUT:-$HOME/.cache/miyu-scripts-migration}
rm -rf "$OUT/home"; mkdir -p "$OUT/home"
export MIYU_HOME=$OUT/home
# 隔离 IPC socket:否则工具桥会连上本机真 daemon,列的是它的会话工具面
mkdir -p "$OUT/runtime"; export XDG_RUNTIME_DIR=$OUT/runtime
export MIYU_SYSTEM_SCRIPTS_DIR=$REPO/src/scripts
pass=0; fail=0
check() { # name json expect-substring
  local name=$1 json=$2 expect=$3 out
  out=$("$BIN" tool-call "$name" "$json" 2>&1)
  if grep -q -- "$expect" <<<"$out" && ! grep -q '"ok": false' <<<"$out"; then
    echo "PASS $name"; pass=$((pass+1))
  else
    echo "FAIL $name"; echo "$out" | head -12; fail=$((fail+1))
  fi
}
check codec '{"op":"hash","input_text":"abc","algorithms":"sha256"}' 'ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad'
check scientific_calculator '{"expression":"2^10 + sqrt(16)"}' '1028'
check divine '{"method":"dice","count":2,"sides":6}' 'rolls'
check online_man '{"action":"read","name":"ls","max_chars":600}' 'LS(1)'
check get_weather '{"location":"Tokyo","days":1}' 'summary'
check query_moegirl '{"query":"初音","mode":"search"}' '初音未来'
check game_compat '{"query":"Elden Ring","max_reports":1}' 'protondb'
check fcitx5_input_method_wiki_qurey '{"topic":"xim","language":"en","include_page_excerpt":false}' 'XMODIFIERS'
check query_deepseek_status '{"include_incidents":false}' 'overall_status'
check read_clipboard '{"preferred_type":"text"}' 'content_type'
echo "== registry listing =="
"$BIN" tool-call --list 2>/dev/null | grep -cE "^(codec|divine|get_weather|game_compat|online_man|query_moegirl|query_deepseek_status|read_clipboard|scientific_calculator|fcitx5_input_method_wiki_qurey)\b" || true
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
