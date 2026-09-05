# 脚本面板真机走查

隔离 home 起一个 daemon，用 Python playwright 走一遍脚本面板：表格 → 抽屉 → 禁用 → 启用 → 补描述注册 → 过滤/搜索，逐步截图到 `shots/`，并把控制台错误与 4xx/5xx 响应汇总成退出码。

```sh
# 1. 隔离 home（不要用真实 ~/.miyu；config 里不配 web 密码即免登录）
H=/tmp/miyu-dash-scripts
mkdir -p "$H/run" "$H/data/scripts"
printf '#!/bin/sh\necho quiet\n' > "$H/data/scripts/quiet.sh"   # 故意缺描述头，供「未注册→注册」流程
mkdir -p "$H/data/scripts/personas/alter"                        # 自定义人格层，供「人格筛选」步骤
printf '#!/bin/sh\n# Description: Alter-only helper.\necho alter\n' > "$H/data/scripts/personas/alter/alter_tool.sh"
# 2. 起 daemon（换一个不与线上 8300 冲突的端口；系统脚本目录指向仓库内置脚本）
MIYU_HOME=$H XDG_RUNTIME_DIR=$H/run MIYU_SYSTEM_SCRIPTS_DIR=$PWD/src/scripts \
  target/debug/miyu __daemon --port 18402 &
# 3. 走查
python3 testkit/dashboard-scripts/shoot.py http://127.0.0.1:18402
```

坑：`XDG_RUNTIME_DIR` 路径太长会撞 `SUN_LEN`；daemon 用 `__daemon` 隐藏子命令直接起，别用 `miyu web`（它会去找/拉线上 daemon）。改了 JS/CSS 必须重新 `cargo build` 再重启，静态资源编进二进制。
