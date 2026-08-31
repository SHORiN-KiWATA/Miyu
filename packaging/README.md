# packaging

各发行版/平台的打包适配，每个平台一个子目录：

- `arch/miyu-git/` — AUR VCS 包（从最新 main 源码构建），AUR `miyu-git` 的真相源
- `arch/miyu-release/` — 发布资产构建器：从 `v<版本>` 标签构建预编译
  `miyu-<版本>-<rel>-x86_64.pkg.tar.zst`，上传到 GitHub Release
- `arch/miyu/` — AUR 二进制包装包（下载上述 Release 资产 + Noto 字体），
  AUR `miyu` 的真相源
- `freebsd/miyu/` — FreeBSD port（`deskutils/miyu`）。首次使用需在
  FreeBSD 机器上 `make makesum` 生成 distinfo（含 521 个 crate 的校验和），
  之后 `make install`。系统资产装到 `/usr/local/share/miyu/`，运行时由
  `src/paths.rs` 的 `system_share_dir()` 按平台前缀解析，亦可用
  `MIYU_SYSTEM_PREFIX` 覆盖

## FreeBSD 移植说明

- 源码本身对非 Linux/macOS 的 Unix 已有回退路径（进程存活检测走
  `kill(pid, 0)`，剪贴板/通知走外部命令），`cargo check
  --target x86_64-unknown-freebsd` 可零警告通过
- 音频（rodio → cpal）只有 ALSA 后端，依赖 FreeBSD 的
  `audio/alsa-lib` 移植（其会把 ALSA 调用翻译到 OSS `/dev/dsp`）
- 技能的原子目录交换（Linux `renameat2` RENAME_EXCHANGE / macOS
  `renameatx_np`）在 FreeBSD 上退化为带回滚的非原子 rename 交换

## 发布流程（Arch）

1. `Cargo.toml` 升版本 → 提交 `release: vX.Y.Z` → 打标签 `vX.Y.Z` → push（含标签）
2. 在干净目录用 `arch/miyu-release/PKGBUILD` 构建：
   `PACKAGER='Miyu Release <noreply@example.com>' makepkg -Cf`
3. `gh release create vX.Y.Z <产物>.pkg.tar.zst --title "Miyu X.Y.Z"`
4. 更新 `arch/miyu/PKGBUILD` 的 `pkgver` 与资产 sha256，
   `arch/miyu-git/PKGBUILD` 刷新 `pkgver` 快照
5. 复制两份 PKGBUILD 到 AUR 检出目录，`makepkg -Cf` 本地实测，
   `makepkg --printsrcinfo > .SRCINFO`，提交 `upd: X.Y.Z` 并 push

## 系统资产约定

除 `/usr/bin/miyu` 外，Miyu 运行时按固定路径查找以下系统资产，
打包时需要一并安装：

| 路径 | 内容 | 来源 | 缺失时的行为 |
|---|---|---|---|
| `/usr/share/miyu/fonts/` | 长回复转图片的渲染字体 | Noto 上游（AUR 包装包下载；发布资产不含字体） | 长文转图静默退化为纯文本 |
| `/usr/share/miyu/memes/miyu/` | 内置表情库 | `src/memes/miyu/` | 默认人格无内置表情 |
| `/usr/share/miyu/default-kb/` | 默认知识库 | 本仓库 `kb/` + Shorin Wiki 仓库，运行时 `miyu update-default-kb` 更新 | 默认知识库为空 |
| `/usr/share/miyu/scripts/` | 系统级脚本 | `src/scripts/` | 无内置脚本 |
