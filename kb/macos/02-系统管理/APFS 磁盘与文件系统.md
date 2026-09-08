# APFS：磁盘、文件系统与快照

macOS 从 High Sierra 起默认用 APFS（Apple File System）。理解它才能看懂磁盘和存储占用。

## 基本概念

- **APFS 容器**：一块物理磁盘/分区被格式化成 APFS 后叫"容器"（Container）。
- **卷（Volume）**：容器里可以有多个卷，共享容器空间。系统装好后你会看到：
  - `Macintosh HD`（系统卷）
  - `Macintosh HD - Data`（数据卷）
  - `Macintosh HD - Updates`（更新卷，平时隐藏）
- **共享空间**：各卷没有固定大小上限，按需从容器里分配，这也是"可用空间"看着会变的原因。
- **快照（Snapshot）**：APFS 支持对卷拍快照，Time Machine 和系统更新都靠它。

## 常用命令

```zsh
diskutil list                       # 列出所有磁盘/卷
diskutil apfs list                  # 看 APFS 容器和卷结构
df -h                               # 看挂载点和占用（人类可读）
diskutil info /                     # 根卷详细信息
```

## 磁盘占用排查

```zsh
du -sh ~/Library 2>/dev/null        # 看某个目录多大
du -sh * | sort -rh | head -20      # 当前目录下最大的 20 个
sudo du -sh /private/var/* | sort -rh | head   # 系统缓存/日志大头
```

大文件快速定位：系统设置 → 通用 → 储存空间，能按类别看；或 `find / -size +1G 2>/dev/null`。

## 查看快照与删除

```zsh
tmutil listlocalsnapshots /         # 列出本地快照
tmutil deletelocalsnapshots 2026-08-01-123456   # 删指定快照
```

本地快照占空间是"可用空间变少"常见元凶，尤其 Time Machine 备份盘不在的时候。

## 常见操作

- **新建卷**：`diskutil apfs addVolume /dev/diskX APFS 卷名`（先 `diskutil list` 找到容器 disk 号）。
- **卸载/挂载**：`diskutil unmount /dev/diskXsY`、`diskutil mount /dev/diskXsY`。
- **修复权限**：APFS 时代一般不需要 `fsck`；启动到恢复模式可用「磁盘工具 → 急救」。

## 注意事项

- 别用 `rm -rf` 删系统卷里的东西，SIP 会挡一部分，乱删容易把系统搞坏。
- 终端里 `sudo diskutil apfs unlock` 之类操作前先看清 disk 编号，别敲错盘。
- APFS 卷无法像 HFS+ 那样直接跨盘拖放，要迁移数据用「迁移助理」或 Time Machine。
