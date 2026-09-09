#!/usr/bin/env bash
# 拆分安全网：每一步搬完代码都跑这个，绿了才提交。
#
# 拆分的铁律是「零行为变化」，而零行为变化没法靠肉眼保证——这个脚本把能
# 机械检查的部分全查一遍：
#
#   1. 格式      改动过的文件违规数不得增加（存量不追，见 fmt_no_regress.py）
#   2. 编译      --all-targets，测试代码也要编过
#   3. 测试      全量；用例数不得减少（搬测试时最容易漏掉一整个 mod）
#   4. 文件规模  不得出现新的越红线文件，超标文件不得变长
#   5. 依赖方向  不得新增跨层引用，已有的不得变多
#
# 关于格式：`cargo fmt --check` 当前有约 4400 行 diff（历史遗留）。全仓格式化
# 会产生一个巨大的、与拆分混在一起的提交，破坏 `git blame` 与 bisect，所以不
# 做。但「只查改动过的文件」也不对——web.rs 在 HEAD 时就有 39 处违规，碰一下
# 就把历史欠账全算到这次头上。于是与另外两道门禁同一语义：只禁止变差。
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

step() { printf '\n\033[1m── %s ──\033[0m\n' "$1"; }

step "格式"
python3 test_scripts/fmt_no_regress.py

step "编译"
cargo check --all-targets

step "测试"
# 数「跑了多少个」而不是「过了多少个」：只数 passed 的话，一个用例失败会被
# 误判成「用例消失」，把两类完全不同的问题混在一个数字里。失败单独判。
before=$(git show HEAD:test_scripts/.test-count 2>/dev/null || echo 0)
#  + 用例失败会让脚本在这里就断掉，判定逻辑根本跑不到——先收下退出
# 码，由下面的逻辑决定放不放行。
# --no-fail-fast:某个 target 失败之后其余 target 照跑。不加的话一个用例
# 失败就少统计好几百个，看起来像「测试消失了」。
output=$(cargo test --no-fail-fast 2>&1 | tee /dev/stderr || true)
now=$(printf '%s\n' "$output" | awk '/^test result:/ {sum += $4 + $6} END {print sum+0}')
failed=$(printf '%s\n' "$output" | awk '/^test result:/ {sum += $6} END {print sum+0}')
echo "$now" > test_scripts/.test-count
if [ "$before" -gt 0 ] && [ "$now" -lt "$before" ]; then
  echo "✗ 用例数从 $before 降到 $now——搬测试时漏了一整个 mod？"
  exit 1
fi
# 这里曾经放行 origin_tty_gates_and_writeback_against_real_pty，理由写的是
# 「依赖本机 PTY 与子进程环境」。那个归因是错的：真凶是它内嵌的那段 Python
# 在拆分模块时被重排掉了缩进，解释器 IndentationError 秒退、没有 stdout，
# Rust 侧 lines.next() 拿到 None 就 panic —— 报错指向 Rust，人就往 Rust 查。
#
# 缩进修好之后这条豁免不但是死的，还留了个盲区:同一类重排再发生一次，门禁
# 会静默放行。所以撤掉——任何用例失败都是红的。
if [ "$failed" -gt 0 ]; then
  echo "✗ 有 $failed 个用例失败"
  exit 1
fi
echo "用例数 $now（基线 $before）"

step "模型面语言"
bash test_scripts/check-model-english.sh

step "文件规模"
python3 test_scripts/refactor_size_report.py --check

step "依赖方向"
python3 test_scripts/arch_dep_check.py

printf '\n\033[32m安全网全绿\033[0m\n'
