# tui-demo：ratatui 全屏 TUI 手感演示（沿用现有 REPL 视觉）

2026-09-10 TUI 重构调研的配套演示。独立 crate，不进主工程依赖，不接 daemon。
画面逐项复刻自现有 REPL（style.rs / footer.rs / layout.rs / inline_picker.rs /
wait_spinner.rs / stream / terminal/kitty.rs），全屏模型只负责让「分页、悬浮、覆盖、自绘选区」成为可能。

## 跑

```sh
cd testkit/tui-demo
CARGO_TARGET_DIR=$HOME/.cache/miyu-tui-demo-target cargo build --release
$HOME/.cache/miyu-tui-demo-target/release/miyu-tui-demo
```

- 启动时重放历史（对应真 REPL 恢复会话的回放）
- 输入 `/`：命令**列表**锚在输入框上方盖住正文（4 行窗口，不带竖条、无按键提示），↑↓ 选、Tab 补全、Enter 执行、Esc 清空
- `/session` `/models` 同样锚在上方（↑↓ 移动、←→ 翻页、直接打字过滤）；`/help` 整页覆盖层；`/img` 放一张 kitty 图片
- 鼠标：程序自己接管拖选——反显和复制都**不含左侧 `┃`**，松开即通过 OSC 52 进剪贴板；滚轮滚历史；
  Shift+拖仍是终端原生选择；F5 关掉捕获可对比
- 排队气泡与输入区之间空一行（对应 resume_at 的 queue_gap）；底部留一行空；右侧 dim 滚动条
- Tab 普通 ⇄ 开发；F4 后台任务条；空闲时不重画

## 测具

```sh
# pyte 模拟终端：逐状态抓屏，注入 SGR 鼠标序列做拖选，截获 OSC 52 剪贴板内容，末尾量 RSS
python3 drive.py $HOME/.cache/miyu-tui-demo-target/release/miyu-tui-demo 110 30 --styles
# 空闲 / 回合中 CPU 与输出字节
python3 cpu_probe.py $HOME/.cache/miyu-tui-demo-target/release/miyu-tui-demo
# 真 kitty（无头 cage）截图：命令列表 / 拖选 / 图片 / 选择器 / 覆盖层；kitty_probe_jobs.py 只测任务行点击（MIYU_DEMO_LOG=文件 记鼠标事件）
OUT=~/.cache/miyu-tui-probe ../kitty-image/run_headless.sh python3 kitty_probe.py $HOME/.cache/miyu-tui-demo-target/release/miyu-tui-demo
```

## 2026-09-10 实测（v4）

- 110×30：RSS 4.0 MB / 匿名 0.6 MB
- 拖选 `┃` 装饰列起、跨三行到 URL 中段，剪贴板得到 `\n\nhttps://www.bilibili.`，不含竖条；输入框内拖选得到 `选中我这`
- 真 kitty 截图 ~/.cache/miyu-tui-probe/tui-*.png

## 版本记录

- v1 自创样式（边框盒子 / 标签栏 / 角色名）→ 用户打回
- v2 复刻现有视觉，选择器居中悬浮
- v3 底部留空、选择器锚在输入框上方、鼠标不捕获、滚动条、kitty 图片、空闲不重画
- v4 `/` 命令列表（用户原意）、自绘拖选不带竖条 + OSC 52、排队气泡空行、启动重放历史
- v5 复制后在输入框上方弹一行通知小悬浮窗（2.5s 自动消失）；输入框文字也可拖选复制
- v10 交互反馈：工具块头行点击展开/收起输出（`  │ ` 行），展开时视口钉住；鼠标悬停到可点击行（任务状态行 / 工具块头）去 dim 提亮；命令列表与选择器悬停即选中、点击即执行
- v9 审美整理：任务详情沿用工具块词汇（`$ 命令` / `  ↳ 补充` / `  │ 输出` / `  ✓ 完成`），标题行左粗体右 dim 元信息两端对齐，说明文字不进正文；命令列表选中行命令粗体、说明正常；去掉右侧滚动条
- v8 输入框上方的悬浮块与输入区隔一行、整行清空；F4 起两条后台任务状态行（命令 + 子代理），点击状态行弹出该任务的详情覆盖区（命令：cwd/命令/最后 6 行输出；子代理：思考摘要与工具进度），Esc 关
- v7 粘贴与占位符：Ctrl+V → `[Image N: …]`，括号粘贴 ≥3 行/≥3 折行 → `[粘贴 N: ~L 行]`，占位符洋红、←→ 整块跳、退格整块删，光标可在输入框内移动（F6 模拟长文本粘贴）；复制通知改为绿边框小盒子 + 绿点
- v6 输入框上方的一切（命令列表 / 选择器 / 通知）不带左侧 ┃；命令列表只占 4 行、无按键提示，选中项超出就滚窗口；spinner 与活动区之间空一行

## 已知简化

- markdown 只做行级 + 简单行内（`**` / 反引号 / URL），表格、公式没做
- 代码高亮是关键字/字符串/数字/注释四类
- 回复、思考摘要、工具块都是本地伪造的定时序列
- ↑↓ 在输入为空时滚历史，与真 REPL 的输入历史冲突，待拍板
- 每帧重建整份历史行（真做要按条目缓存）
