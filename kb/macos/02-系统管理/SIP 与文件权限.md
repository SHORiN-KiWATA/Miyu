# SIP 系统完整性保护与文件权限

SIP（System Integrity Protection）保护系统目录和进程不被随意改动，是 macOS 安全基石。

## 什么是 SIP

- 保护 `/System`、`/usr`、`/bin`、`/sbin` 等目录，以及系统进程。
- 即使 root 也不能直接改受保护区域。
- 部分 App 安装报错、改系统文件失败，常见原因就是 SIP。

## 查看 SIP 状态

```zsh
csrutil status
# System Integrity Protection status: enabled.
```

## 临时关闭（不推荐）

需要进恢复模式操作：

1. 关机 → 长按电源键进恢复模式（Apple Silicon）。
2. 菜单栏 → 实用工具 → 终端。
3. `csrutil disable` → 重启。

关闭后系统安全性下降，除非调试系统级工具，否则别关。用完了记得 `csrutil enable` 开回来。

## 文件权限基础

```zsh
ls -l 文件            # 看权限
chmod 755 文件        # rwxr-xr-x
chmod +x 文件         # 加执行权限
chown 用户:组 文件     # 改所有者（需 sudo）
```

## 权限相关命令

```zsh
# 看当前用户
id -un
# 查某目录权限问题
ls -ld /path/to/dir
# 递归修复（谨慎！会覆盖自定义权限）
chmod -R 755 /path
```

## 常见问题

- App 提示「无法打开，因为无法验证开发者」：右键打开，或系统设置 → 隐私与安全性 → 仍要打开；这是 Gatekeeper 不是权限问题。
- 改不了 /usr 下文件：SIP 保护，别硬来；用 Homebrew 装到 /opt/homebrew。
- 文件夹权限乱了：优先改回自己目录（`~/`）下文件，系统目录别动。

## 恢复默认

SIP 被关过又想恢复：恢复模式里 `csrutil enable`。部分功能（如定位）会要求完全开启。

注意：文件权限改动前先备份，`chmod -R` 全目录扫一遍容易误伤。
