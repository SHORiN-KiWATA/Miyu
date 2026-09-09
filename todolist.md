- 减少token消耗
- 重写完整TUI
- REPL 复制不带左侧装饰
- REPL 回车提交时光标瞬移到左下角再回输入框(kitty)。已排查:提交同步块内的
  ESC[6n 查询已全部消除(cursor_position 推算/块外查询),严格同步语义下 pyte
  逐帧重放测不到可见底部光标帧,真机仍可复现——嫌疑转向回合渲染循环或 kitty
  同步实现细节。排查记录:memory miyu-qq-fixes-2026-08-20
- 优化占用，提高运行效率和稳定性
- 多平台字体处理
- 图片渲染不再依赖chafa
- 优化io性能
- computer use
- 优化compact效果
- 增强webui成果交付能力
- 多发行版打包，MacOS适配

- webui圆角的radius统一为8
- 在已有goal运行过程中无法发送follow up消息排队。在claudecode里是可以的，但是我们的不可以。![image-20260909095910775](/home/shorin/.config/Typora/typora-user-images/image-20260909095910775.png)

- 在 webui 的 dashboard 刷新页面会回到 chat 页面而不是在刷新前的那个 dashboard
- 撤回工具的日志里没有带上那条消息是谁发的，只有消息id和理由![image-20260909112351473](/home/shorin/.config/Typora/typora-user-images/image-20260909112351473.png)
- 有人反馈，webui数据统计页面供应商 id 修改后数据统计不会改变，刷新也是。![img](file:////home/shorin/.config/QQ/nt_qq_682ca15bb1c90b309582ba81567485e2/nt_data/Pic/2026-09/Ori/2a1c694acc44e64bf95f3eee6b5e8c63.png)这个 deepseek- 改成了日日新，但是刷新不变
- webui 设置页面在网页最大化或者宽度比较宽的时候不居中![image-20260909121722789](/home/shorin/.config/Typora/typora-user-images/image-20260909121722789.png)

## Feats

- Live2D
- 支持QQ官方机器人
- 支持telegram
- 安全性、权限
