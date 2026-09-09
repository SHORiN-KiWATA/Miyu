# 发布说明草稿（合并后追加进主检出的 next-release-note.md）

## 修复

- **中转线（Claude Code / Codex / Antigravity）的上下文压缩现在能记住碰过的文件**：以前这三条线上模型读写的文件从不进压缩摘要的 `<read-files>/<modified-files>`，压后回灌也永远是空的；现在原生 Read/Edit/Write（以及 agy 的 view_file/write_to_file、codex 的文件改动）都会被记下来，压缩后最近改过的文件会重新灌回上下文。
- **Claude Code 线不再把窗口透传给 CLI 自压缩**：压缩统一由 Miyu 做，两套压缩不再抢跑；之前的透传实际上从没生效过（Miyu 总是先到）。
