# niri v26.04 版本更新说明与核心特性总结

**元数据 (Metadata)**
- **软件名称**: niri (Wayland 复合管理器 / Scrollable Tiling Window Manager)
- **版本号**: v26.04
- **来源链接**: https://github.com/niri-wm/niri/releases/tag/v26.04
- **相关标签**: Linux, Wayland, Window Manager, niri, Release Notes

## 概述
niri v26.04 是一个带来了大量重磅新功能、底层架构重构和体验优化的重要版本。本次更新在视觉效果、交互体验、屏幕录制、底层渲染性能以及老硬件兼容性上均有显著提升。

## 1. 项目新动态
- **迁移至组织账号**：niri 现已从开发者个人账号迁移至正式的 GitHub 组织 (`niri-wm`) 下，以便更好地管理问题分类和分配团队权限。
- **依赖要求与里程碑**：最低支持的 Rust 版本提升至 1.85，同时主仓库星标 (Stars) 数量已突破 20,000。

## 2. 重磅新特性：背景模糊 (Blur)
- **原生支持**：呼声最高的功能落地，niri 现在原生支持背景模糊效果。
- **Xray 模糊（默认选项）**：采用极其高效的实现方式，仅对壁纸进行一次模糊处理后作为静态图像复用，大幅减少 GPU 性能消耗。
- **普通模糊与多场景应用**：可通过 `window-rule` 和 `layer-rule` 手动为特定应用或顶层组件开启普通模糊或 Xray 模糊，模糊效果支持应用到弹出菜单（Pop-ups）。
- **协议支持**：支持 `ext-background-effect` Wayland 协议，客户端（如 foot、kitty、Ghostty 等终端）可直接向 compositor 请求模糊效果。

### 以下是Shorin的Blur配置

```
// --- NIRI BLUR START ---
// 顶层的blur配置，细节调整blur的效果
blur {
    passes 3
    offset 3
    noise 0.02
    saturation 1.5
}
// 全局窗口规则。让所有普通窗口以xray的形式显示blur。xray仅渲染一次模糊版本的背景，然后将其以类似“壁纸”的形式显示在窗口后面，不是实时渲染模糊，所以完全没有性能消耗。
window-rule {
    background-effect {
        xray true
        blur true
    }
}
// 浮动窗口禁用xray可以实时渲染模糊效果但是性能消耗高，开启xray会穿过浮动窗口底下的窗口直接透视到桌面。看个人喜好选择禁用与否吧。
window-rule {
    match is-floating=true
    background-effect {
        xray true
        blur true
    }
}
// fuzzle专属的layer规则
layer-rule {
    match namespace="^launcher$"
    geometry-corner-radius 8
    background-effect {
        xray false
	blur true
    }
}
// --- NIRI BLUR END ---


```


## 3. 配置与交互体验提升
- **配置文件的可选引入 (Optional Includes)**：配置支持 `include optional=true "path"`。若文件不存在仅输出警告而不报错失败，适合 NixOS 部署或本地覆盖配置，路径支持 `~` 展开。
- **滚动时的指针环绕 (Pointer warping)**：拖拽窗口水平滚动视图时，鼠标指针到达屏幕边缘会自动从另一侧穿出，多窗口滚动浏览更流畅自然。
- **支持取消拖拽**：在进行拖拽操作时，按下 `Escape` 键即可直接取消拖放。

## 4. 屏幕录制与共享优化 (Screencasting)
- **独立鼠标指针元数据**：通过 PipeWire 录屏时，鼠标指针不再硬编码绘制在视频流上，而是作为独立元数据发送。这允许在 OBS 等软件中自由控制指针可见性，并实现了仅在悬停目标窗口时显示指针。
- **修复黑屏异常**：动态录屏目标 (Dynamic cast target) 会延迟启动视频流，直到用户实际选定目标，修复了 Microsoft Teams 因收到 1x1 像素黑屏而崩溃或异常的问题。
- **新增 Cast IPC**：使用 `niri msg casts` 可查看所有活跃录屏进程，允许第三方状态栏构建录屏指示器，支持通过 IPC 强制停止录制。

## 5. 动画与输入法 (IME) 修复
- **弹出窗口输入法兼容**：放宽事件抢占机制，解决了使用 Fcitx5 等输入法时，GTK 4 弹出窗口（如重命名文件弹窗）因输入法抢占焦点而瞬间关闭的顽疾。
- **修复动画顿挫**：修复了窗口取消全屏、取消最大化或被拖出状态变为浮动时，原本应同步进行的水平滚动动画失效或不连贯的问题。

## 6. 底层性能优化与老硬件支持
- **渲染架构大重构 (Push-based Rendering)**：渲染代码从基于“拉取”（Iterators）的模式彻底重构为基于“推送”（Push闭包）的模式。消除了大量临时内存分配，主流设备渲染列表构建速度提升 2~3 倍，极旧设备提升达 8 倍。
- **GPU 性能分析**：集成 Tracy GPU profiling，开发者可直观分析渲染帧在多 GPU 环境及系统底层的性能表现。
- **老硬件兼容性**：修复旧版 Intel 显卡笔记本截图问题（Smithay 库 OpenGL 枚举修复）；通过着色器优化，成功在十多年前的上网本（如 ASUS Eee PC）上运行圆角和动画。

## 7. 其他改善与修复
- 嵌套运行的 niri 现在支持 DMA-BUF 硬件加速。
- 实现 `ext-foreign-toplevel-list` Wayland 协议，方便任务栏和 Dock 组件获取并管理窗口 ID。
- 修复高回报率鼠标（High Hz mouse）在休眠超时场景下可能导致客户端崩溃或性能骤降的问题。
- 为数位板/手绘板新增 `map-to-focused-output` 选项，支持动态映射到当前活动显示器。

