# WebUI 六项修复的 A/B 走查(09-10)

沙箱 daemon + OpenAI 桩(`stub_reasoning.py`,MODE=job:起后台任务 → 工具结果后长回复
→ 任务完成的唤醒回合再长回复)+ Playwright(Chromium,python 版)。

```sh
BIN=target/release/miyu TAG=new python3 testkit/webui-fixes/webui_verify.py
FLAKY_SSE=0 ...   # 不模拟两秒断网
```

| 判定项 | 修前 | 修后 |
|---|---|---|
| ctx_after_reload(会话钉 stub-b 50k,刷新后上下文条仍 50k) | 100k ✗ | 50k ✓ |
| ctx_after_settings(拉一次 /api/config 后仍 50k) | ✗ | ✓ |
| enter_newline(触屏视口回车换行不发送) | 发送了 ✗ | 换行 ✓ |
| ctrl_enter_sends | ✓ | ✓ |
| usage_stacked(390px 用量页环形图在表格上方) | 并排 ✗ | 竖排 ✓ |
| jump_offset_synced(撑高 dock 后按钮 bottom 跟上) | 172 vs 205 ✗ | ✓ |
| autoscroll_kept(唤醒回合流式中整段重建后仍贴底) | ✓(未复现) | ✓ |

自动滚动那条:三种时序(唤醒回合在空闲时起跑 / 任务在流式中间完成 / 中途断网两秒
再回来)在无头 Chromium 上都没复现出原始现象,修复按代码推断(`renderConversation`
重建时的滚动写入加程序滚动守卫),真机效果待用户验证。

产物:`~/.cache/miyu-arch-fixes/webui-<TAG>/{report.json,daemon.log,*.png}`。
