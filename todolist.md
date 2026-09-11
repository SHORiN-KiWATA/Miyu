## Main

- webui artifact 左键拖拽放大后的内容有迟滞感，还会错位，很异常。
- 普通用户没法上传附件![image-20260911183616601](/home/shorin/.config/Typora/typora-user-images/image-20260911183616601.png)

- ![image-20260911195614755](/home/shorin/.config/Typora/typora-user-images/image-20260911195614755.png) 任务的效果好丑；另外，开发模式的子代理应该显示为 开发代理，普通子代理显示为 任务代理；时间边上应该有token消耗显示，每步更新
- 移除goal的轮数限制
- 这张图是AI的反馈，子代理不落盘导致resume不可靠![image-20260911231913240](/home/shorin/.config/Typora/typora-user-images/image-20260911231913240.png)

- goal状态行的编辑框只有一行，写起来不方便，看起来也不方便![image-20260911232015564](/home/shorin/.config/Typora/typora-user-images/image-20260911232015564.png)





## Feats

- 多发行版打包工作流，MacOS适配
- Live2D
- 支持QQ官方机器人
- 支持telegram
- 安全性、权限
- 首次使用TUI OOBE

## 优化

优化数据统计页面，提升可读性和美观度

减少token消耗

重写完整TUI

多平台字体处理

优化io性能

优化占用，提高运行效率和稳定性

减少dev模式AI看到的提示词，以让AI尽可能发挥自身的能力而不受影响

沙盒和webui文件分享、附件、上传功能的兼容性

## 搁置

- REPL 复制不带左侧装饰
