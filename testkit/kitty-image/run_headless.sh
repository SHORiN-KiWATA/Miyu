#!/usr/bin/env bash
# 在无头 cage(wlroots headless 后端)里起一个真 kitty 跑指定命令,不碰用户桌面。
# kitty 带远程控制(KITTY_LISTEN_ON),里面的脚本可以用 `kitten @ action ...` 滚视口,
# 用 grim 截图(截的是无头输出)。
#
# 用法:
#     testkit/kitty-image/run_headless.sh python3 testkit/kitty-image/ghost_probe.py
#     OUT=~/.cache/miyu-kitty-probe testkit/kitty-image/run_headless.sh <cmd...>
set -u
export OUT="${OUT:-$HOME/.cache/miyu-kitty-probe}"
mkdir -p "$OUT"
export WLR_BACKENDS=headless
export WLR_LIBINPUT_NO_DEVICES=1
export WLR_RENDERER="${WLR_RENDERER:-pixman}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
unset DISPLAY
unset WAYLAND_DISPLAY
# 无头输出的尺寸:大一点好放下 Miyu 的活动区。
export WLR_HEADLESS_OUTPUTS=1
exec cage -- kitty \
    -o allow_remote_control=yes \
    -o font_size="${KITTY_FONT_SIZE:-11}" \
    -o scrollback_lines=2000 \
    --listen-on "unix:@miyu-kitty-probe-$$" \
    "$@"
