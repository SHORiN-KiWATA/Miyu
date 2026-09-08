- 减少token消耗

- 重写完整TUI
- REPL 复制不带左侧装饰
- REPL 回车提交时光标瞬移到左下角再回输入框(kitty)。已排查:提交同步块内的
  ESC[6n 查询已全部消除,pyte 逐帧重放(严格同步语义)测不到可见底部光标帧,
  真机仍复现——嫌疑转向回合渲染循环或 kitty 同步实现细节。排查记录:
  memory miyu-qq-fixes-2026-08-20
- REPL 回车提交时光标瞬移到左下角再回输入框(kitty)。已排查:提交同步块内的
  ESC[6n 查询已全部消除(cursor_position 推算/块外查询),严格同步语义下 pyte
  逐帧重放测不到可见底部光标帧,真机仍可复现——嫌疑转向回合渲染循环或 kitty
  同步实现细节。排查记录:memory miyu-qq-fixes-2026-08-20
- 优化占用，提高运行效率和稳定性
- 多平台字体处理
- 图片渲染不再依赖chafa
- 优化io性能
- computer use

## Feats

- MacOS适配
- Live2D
- 支持QQ官方机器人
- 支持telegram
- 安全性、权限
