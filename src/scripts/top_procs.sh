#!/bin/bash
# description: Show top processes by CPU or memory usage. Arguments are passed as JSON via stdin: {"sort":"cpu"|"mem","n":10}. Default: CPU sort, 10 entries.
# Parameters are read from stdin as JSON.

sort="cpu"
n=10

if [ ! -t 0 ]; then
  input=$(cat)
  if [ -n "$input" ]; then
    s=$(echo "$input" | sed -n 's/.*"sort"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')
    [ -n "$s" ] && sort="$s"
    num=$(echo "$input" | sed -n 's/.*"n"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
    [ -n "$num" ] && n="$num"
  fi
fi

case "$sort" in
  mem|memory) sort_key=4 ;;
  *) sort_key=3 ;;
esac

ps -eo user,pid,pcpu,pmem,comm --no-headers | sort -k"$sort_key" -rn | head -n "$n" | \
awk -v n="$n" -v sort_label="$sort" '
BEGIN {
  if (sort_label == "mem") label = "内存"
  else label = "CPU"
  printf "进程占用排行（按 %s 排序，Top %d）\n\n", label, n
  i = 1
}
{
  printf "%d. %s (PID %s)\n", i, $5, $2
  printf "   CPU %s%%  MEM %s%%  USER %s\n\n", $3, $4, $1
  i++
}
'
