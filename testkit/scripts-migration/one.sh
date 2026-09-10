#!/usr/bin/env bash
# 单件直调:BIN=<miyu> bash one.sh <tool> '<json>'
set -u
BIN=${BIN:-target/release/miyu}
REPO=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${OUT:-$HOME/.cache/miyu-scripts-migration}
mkdir -p "$OUT/home"
export MIYU_HOME=$OUT/home
export MIYU_SYSTEM_SCRIPTS_DIR=$REPO/src/scripts
"$BIN" --help 2>&1 | grep -i "tool" | head -3
echo "--- calling: $1"
timeout 60 "$BIN" tool-call "$1" "$2" 2>&1 | head -${LINES_MAX:-20}
echo "exit=${PIPESTATUS[0]}"
