# Mac 机型与配置大全

截至 2026 年 8 月发布的 Apple Silicon Mac 全机型速查（M5 系列已发布：M5 基础版 2025-10 首发于 14" MacBook Pro，M5 Pro/Max 2026-03 随 MacBook Pro 更新）。识别自己的机器：系统设置 → 通用 → 关于本机（或 `system_profiler SPHardwareDataType`）。

## 芯片总览（Apple Silicon 各代）

| 芯片 | 发布 | CPU 核数 | GPU 核数 | 统一内存上限 | 内存带宽 |
|---|---|---|---|---|---|
| M1 | 2020 | 8（4P+4E） | 7/8 | 16 GB | 68.25 GB/s |
| M1 Pro | 2021 | 8/10 | 14/16 | 32 GB | 200 GB/s |
| M1 Max | 2021 | 10 | 24/32 | 64 GB | 400 GB/s |
| M1 Ultra | 2022 | 20 | 48/64 | 128 GB | 800 GB/s |
| M2 | 2022 | 8（4P+4E） | 8/10 | 24 GB | 100 GB/s |
| M2 Pro | 2023 | 10/12 | 16/19 | 32 GB | 200 GB/s |
| M2 Max | 2023 | 12 | 30/38 | 96 GB | 400 GB/s |
| M2 Ultra | 2023 | 24 | 60/76 | 192 GB | 800 GB/s |
| M3 | 2023 | 8（4P+4E） | 8/10 | 24 GB | 100 GB/s |
| M3 Pro | 2023 | 11/12 | 14/18 | 36 GB | 150 GB/s |
| M3 Max | 2023 | 14/16 | 30/40 | 128 GB | 300–400 GB/s |
| M3 Ultra | 2025 | 28（20P+8E） | 80 | 512 GB | 819 GB/s |
| M4 | 2024 | 10（4P+6E） | 10 | 32 GB | 120 GB/s |
| M4 Pro | 2024 | 14（10P+4E） | 20 | 64 GB | 273 GB/s |
| M4 Max | 2024 | 16（12P+4E） | 40 | 128 GB | 546 GB/s |
| M5 | 2025 | 9/10（3–4S+6E） | 8/10 | 32 GB | 153 GB/s |
| M5 Pro | 2026 | 15/18（5–6S+10–12P） | 16/20 | 64 GB | 307 GB/s |
| M5 Max | 2026 | 18（6S+12P） | 32/40 | 128 GB | 460/614 GB/s |

M5 系列备注：M5 为单 die，M5 Pro/Max 采用 Fusion Architecture 双 die 融合；制程 TSMC N3P 3nm，LPDDR5X 9600 MT/s；16 核神经网络引擎（NPU 42 TOPS）。

## MacBook Air

轻薄无风扇本，适合办公与日常。

| 机型 | 年份 | 芯片 | 屏幕 | 内存 | 接口 |
|---|---|---|---|---|---|
| MacBook Air M1 | 2020 | M1 | 13.3" Retina | 8–16 GB | 2×雷雳 3 |
| MacBook Air M2 | 2022 | M2 | 13.6" 刘海屏 | 8–24 GB | 2×雷雳 4、MagSafe |
| MacBook Air M3 | 2024 | M3 | 13.6" / 15.3" | 8–24 GB | 2×雷雳 4、MagSafe |
| MacBook Air M4 | 2025 | M4 | 13.6" / 15.3" | 16–32 GB | 2×雷雳 4、MagSafe、1200 万像素摄像头 |
| MacBook Air M5 | 2026 | M5 | 13.6" / 15.3" | 16–32 GB | 2×雷雳 4、MagSafe、12MP Center Stage、Wi-Fi 7 |

## MacBook Pro

专业本，14" 与 16" 两档。

| 机型 | 年份 | 芯片 | 屏幕 | 内存 | 接口 |
|---|---|---|---|---|---|
| MacBook Pro M1 Pro/Max | 2021 | M1 Pro/Max | 14.2" / 16.2" | 16–64 GB | 3×雷雳 4、HDMI、SDXC、MagSafe |
| MacBook Pro M2 Pro/Max | 2023 | M2 Pro/Max | 14.2" / 16.2" | 16–96 GB | 同上 |
| MacBook Pro M3 系列 | 2023–2024 | M3 / M3 Pro / M3 Max | 14" / 16" | 8–128 GB | 同上 |
| MacBook Pro M4 系列 | 2024–2025 | M4 / M4 Pro / M4 Max | 14" / 16" | 16–128 GB | M4 Pro/Max 为 3×雷雳 5、HDMI 2.1、SDXC、MagSafe |
| MacBook Pro M5 | 2025 | M5 | 14" | 16–32 GB | 3×雷雳 4、HDMI、SDXC、MagSafe |
| MacBook Pro M5 Pro/Max | 2026 | M5 Pro / M5 Max | 14" / 16" | 24–128 GB | 3×雷雳 5、HDMI、SDXC、MagSafe、Wi-Fi 7 |

## iMac

一体机，24" 起。

| 机型 | 年份 | 芯片 | 屏幕 | 内存 | 接口 |
|---|---|---|---|---|---|
| iMac M1 | 2021 | M1 | 24" 4.5K | 8–16 GB | 2×雷雳 4 |
| iMac M3 | 2023 | M3 | 24" 4.5K | 8–24 GB | 2×雷雳 4 |
| iMac M4 | 2024 | M4 | 24" 4.5K（可选纳米纹理） | 16–32 GB | 2 或 4×雷雳 4 |

> 截至 2026-08 暂无 M5 版 iMac，M4 款仍在售。

## Mac mini

桌面小主机，性价比之选。

| 机型 | 年份 | 芯片 | 内存 | 接口 |
|---|---|---|---|---|
| Mac mini M1 | 2020 | M1 | 8–16 GB | 2×雷雳 4、HDMI、2×USB-A、千兆/10GbE |
| Mac mini M2/M2 Pro | 2023 | M2 / M2 Pro | 8–32 GB | M2：2×雷雳 4；M2 Pro：4×雷雳 4 |
| Mac mini M4/M4 Pro | 2024 | M4 / M4 Pro | 16–64 GB | 前面板 2×USB-C+耳机孔；M4：3×雷雳 4；M4 Pro：3×雷雳 5 |

> 截至 2026-08 暂无 M5 版 Mac mini（传闻 2026 下半年，未官宣）。

## Mac Studio

工作站级桌面，视频剪辑/开发重负载。

| 机型 | 年份 | 芯片 | 内存 | 接口 |
|---|---|---|---|---|
| Mac Studio M1 Max/Ultra | 2022 | M1 Max / M1 Ultra | 32–128 GB | 前面 2×USB-C+SDXC；背面 4×雷雳 4、HDMI、10GbE |
| Mac Studio M2 Max/Ultra | 2023 | M2 Max / M2 Ultra | 32–192 GB | 同上 |
| Mac Studio M4 Max/M3 Ultra | 2025 | M4 Max / M3 Ultra | 36–512 GB | 前面 2×USB-C+SDXC；背面 4×雷雳 5、HDMI、10GbE |

> 截至 2026-08 暂无 M5 版 Mac Studio（M5 Ultra 传闻 2026 下半年，未官宣）。

## Mac Pro

最高端塔式工作站，支持 PCIe 扩展。

| 机型 | 年份 | 芯片 | 内存 | 接口 |
|---|---|---|---|---|
| Mac Pro M2 Ultra | 2023 | M2 Ultra | 64–192 GB | 6×PCIe 扩展槽、背面 6×雷雳 4、双 10GbE |

> 截至 2026-08 无更新。

## 历史机型（Intel 时代，部分仍常见）

| 机型 | 处理器 | 说明 |
|---|---|---|
| MacBook Air（Intel） | i3/i5/i7 | 2020 年 M1 发布前最后一款，13.3" 无刘海 |
| MacBook Pro 13"（Intel） | i5/i7 | 2020 款最后一代 Touch Bar 版 |
| MacBook Pro 16"（Intel） | i7/i9 | 2019 款，键盘回归剪刀式 |
| iMac 27"（Intel） | i5/i9 | 2020 款 5K，支持用户自行加内存（已停产） |
| iMac Pro | Xeon | 2017–2021，27" 5K 工作站 |

## 选购与识别

- **查看本机**：系统设置 → 通用 → 关于本机；命令行 `system_profiler SPHardwareDataType`。
- **芯片 vs 机型**：M 系列芯片决定性能档位（Pro/Max/Ultra 越往上越强）；机型决定便携性与屏幕。
- **M5 系列现状**：截至 2026-08 已覆盖 MacBook Air / MacBook Pro 两条笔记本线；iMac、Mac mini、Mac Studio、Mac Pro 尚无 M5 款。
- **内存提示**：Apple Silicon 内存不可升级，买前按未来 3–5 年需求定（日常 16 GB、开发 32 GB、重度 64 GB+）。
- **接口提示**：雷雳 4/5 兼容 USB-C；雷雳 5 向下兼容雷雳 4/3。注意 2025 款 14" MacBook Pro（M5 基础款）仍是雷雳 4，只有 M5 Pro/Max 款上雷雳 5。
