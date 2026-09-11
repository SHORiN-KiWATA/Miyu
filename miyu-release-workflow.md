# Miyu 发布流程

真相源是仓库里的 `packaging/README.md` 与 `packaging/arch/*/PKGBUILD`，本文是照着实际
走过的一遍（0.5.0 / 0.5.0-2，2026-09-06）写的操作手册。四个 PKGBUILD 的分工：

| 目录 | 作用 |
|---|---|
| `packaging/arch/miyu-release/` | 发布资产构建器：从源码 `makepkg` 出 `miyu-<ver>-<rel>-x86_64.pkg.tar.zst` 和 `miyu-voice-<ver>-<rel>-x86_64.pkg.tar.zst`，上传到 GitHub Release |
| `packaging/arch/miyu/` | AUR `miyu`：下载上面的 `miyu` 资产转包（用户不本地编译） |
| `packaging/arch/miyu-voice/` | AUR `miyu-voice`：下载 `miyu-voice` 资产转包，依赖 `miyu` |
| `packaging/arch/miyu-git/` | AUR `miyu-git`：从最新 main 源码构建，`pkgver` 由 `pkgver()` 自算 |

资产里已含：二进制、三套字体与许可证、内置表情、系统脚本、默认知识库（本仓库 `kb/` +
Shorin Wiki，Wiki 从 GitHub remote 拉取）、内置 embedding 模型。`miyu-voice` 只有
`/usr/bin/miyu-voice`。

## 前提

- 在 `main` 上，要发的内容已 commit 并 push（`miyu-release` 按 GitHub 上的 tag / commit clone，
  本地未推送的改动进不了包）。
- 装有 `cargo`、`makepkg`、`gh`（已登录）、`bsdtar`；`sudo pacman` 免密。
- AUR 检出：`~/Documents/aur/miyu`、`~/Documents/aur/miyu-voice`、`~/Documents/aur/miyu-git`。
- `sar`（Shorin Arch Repo CLI）在 `~/.local/bin/sar`，pkglist 里 `miyu` / `miyu-voice` 标为 AUR 来源。
- 面向用户的更新说明平时累积在仓库根目录 `next-release-note.md`，发布时搬进 release
  正文，发完清空（保留首行说明）。

## 正式版本（X.Y.Z）

### 1. 升版本、打标签、推送

```bash
cd ~/Documents/github/Miyu
# Cargo.toml: version = "X.Y.Z"
# packaging/arch/miyu-release/PKGBUILD: pkgver=X.Y.Z, pkgrel=1
# packaging/arch/miyu/PKGBUILD 与 miyu-voice/PKGBUILD: pkgver=X.Y.Z, pkgrel=1, _release_pkgrel=1
cargo build --release            # 让 Cargo.lock 跟上版本号
git add Cargo.toml Cargo.lock packaging/arch
git commit -m "release: vX.Y.Z"
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z
```

### 2. 构建发布资产

在干净目录用 `miyu-release` 的 PKGBUILD 构建。全量 release 编译约 8 分钟、内存峰值 11G，
必须用 systemd 用户单元跑（Claude Code / 终端会话重启会连坐杀掉子进程），并限制内存
防止拖死整机：

```bash
B=~/.cache/miyu/release-build-X.Y.Z
mkdir -p "$B"
cp ~/Documents/github/Miyu/packaging/arch/miyu-release/PKGBUILD "$B/"
# sherpa-onnx 静态库归档可从上次构建目录复制过来省下载：sherpa-onnx-v*.tar.bz2
systemd-run --user --collect --unit=miyu-relbuild \
  -p MemoryMax=12G -p MemorySwapMax=0 \
  -E PACKAGER='Miyu Release <noreply@example.com>' -E LC_ALL=C.UTF-8 \
  --working-directory="$B" bash -c 'makepkg -Cf > makepkg.log 2>&1'
# 等它结束
until ! systemctl --user is-active --quiet miyu-relbuild; do sleep 5; done
tail -3 "$B/makepkg.log"        # 期待 "Finished making: miyu X.Y.Z-1"
sha256sum "$B"/*.pkg.tar.zst
```

抽查：`bsdtar -tf "$B/miyu-X.Y.Z-1-x86_64.pkg.tar.zst" | grep -E 'bin/miyu$|fonts/|models/'`，
以及 `bsdtar -xOf ... usr/bin/miyu | grep -c -aF '<本次改动里的新字符串>'` 确认装的是新代码。

### 3. 创建 GitHub Release

正文从 `next-release-note.md` 整理，风格：`## 重要更新` / `## 修复` / `## 安装` 三节，每条
**加粗标题** 加一两句用户视角说明，不写文件名与实现细节。两个资产都要传。

```bash
gh release create vX.Y.Z \
  "$B/miyu-X.Y.Z-1-x86_64.pkg.tar.zst" \
  "$B/miyu-voice-X.Y.Z-1-x86_64.pkg.tar.zst" \
  --title "Miyu X.Y.Z" --notes-file /path/to/notes.md
```

### 4. 回填 sha256 与 miyu-git 快照

```bash
cd ~/Documents/github/Miyu
# packaging/arch/miyu/PKGBUILD        sha256sums=('<miyu 资产 sha256>')
# packaging/arch/miyu-voice/PKGBUILD  sha256sums=('<miyu-voice 资产 sha256>')
# packaging/arch/miyu-git/PKGBUILD    pkgver=X.Y.Z.r<git rev-list --count main>.g<短 sha>
git add packaging/arch
git commit -m "packaging: X.Y.Z 资产 sha256 + miyu-git 快照"
git push origin main
```

### 5. 更新三个 AUR 包

`miyu` 与 `miyu-voice` 本地 `makepkg` 一遍（只是下载资产转包，一分钟内），`miyu-git`
只刷 `.SRCINFO`：

```bash
R=~/Documents/github/Miyu/packaging/arch
for p in miyu miyu-voice; do
  cd ~/Documents/aur/$p
  cp $R/$p/PKGBUILD PKGBUILD
  rm -rf pkg/ src/ *.pkg.tar.zst
  makepkg -Cf
  makepkg --printsrcinfo > .SRCINFO
  git add PKGBUILD .SRCINFO && git commit -m "upd: X.Y.Z" && git push origin master
done
cd ~/Documents/aur/miyu-git
cp $R/miyu-git/PKGBUILD PKGBUILD
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO && git commit -m "upd: X.Y.Z" && git push origin master
```

### 6. 本机安装并轮换 daemon

轮换会掐断进行中的回合（已裁定可以直接打断）。daemon 必须由 systemd 用户单元托管，
不要在 Claude Code 会话里 `miyu daemon start`（会话重启时一起被杀）；带 LANG 否则通知
变英文。

```bash
sudo pacman -U ~/Documents/aur/miyu/miyu-X.Y.Z-1-x86_64.pkg.tar.zst \
               ~/Documents/aur/miyu-voice/miyu-voice-X.Y.Z-1-x86_64.pkg.tar.zst
rm -f ~/.local/bin/miyu                      # 开发期放的测试二进制会遮蔽 /usr/bin/miyu
systemctl --user stop miyu-daemon 2>/dev/null; /usr/bin/miyu daemon stop 2>/dev/null
systemd-run --user --collect --unit=miyu-daemon \
  -E LANG=zh_CN.UTF-8 -E LANGUAGE=zh_CN:en /usr/bin/miyu __daemon --port 8300
P=$(ss -tlnp | grep 8300 | sed 's/.*pid=\([0-9]*\).*/\1/'); readlink /proc/$P/exe   # 应为 /usr/bin/miyu
tail -3 ~/.miyu/cache/logs/miyu.$(date -u +%F).log   # 看到 OneBot 客户端已连接
```

### 7. shorin-arch 源

AUR 生效要几分钟，之后：

```bash
sleep 300 && sar all miyu miyu-voice
```

### 8. 收尾

- `next-release-note.md` 清掉已发布内容，只留首行说明。
- 记忆里登记版本与部署时间。

## 补丁重发（X.Y.Z-2，标签不动）

适用于发版后马上修的小问题，不想升版本号。做法与正式版一样，只有这些差异：

1. 修复合进 main 并 push；**不动 tag**。
2. `packaging/arch/miyu-release/PKGBUILD`：`pkgrel=2`，`source` 里 miyu 那条从
   `#tag=vX.Y.Z` 改成 `#commit=<main 上的完整 sha>`，加一行注释说明缘由。
3. `packaging/arch/miyu/PKGBUILD` 与 `miyu-voice/PKGBUILD`：`pkgrel=2`、`_release_pkgrel=2`
   （资产文件名里的 rel 由它决定）、sha256 回填。`miyu-git` 快照照常刷。
4. 资产按新文件名上传到**同一个** Release，旧的 -1 资产留着：
   `gh release upload vX.Y.Z "$B"/miyu-X.Y.Z-2-*.pkg.tar.zst "$B"/miyu-voice-X.Y.Z-2-*.pkg.tar.zst`
5. Release 正文在 `## 安装` 前插一节 `## X.Y.Z-2 补丁（日期）`，列本次改动：
   `gh release edit vX.Y.Z --notes-file notes.md`（先 `gh release view vX.Y.Z --json body -q .body` 取原文再拼）。
6. 其余（AUR 三包 `upd: X.Y.Z-2`、本机 `pacman -U`、daemon 轮换、`sar all`）同上。

## 注意事项

- **.SRCINFO** 必须随 PKGBUILD 一起推，否则 AUR 不更新。
- **`~/.local/bin/miyu` 遮蔽**：装完包一定删掉，并用 `readlink /proc/<8300 持有者>/exe` 确认。
- **无主文件冲突**：曾手动 `sudo cp` 进 `/usr/share/miyu/` 的文件会挡 `pacman -U`，确认内容一致后
  `--overwrite '/usr/share/miyu/*'` 让包接管。
- **`miyu` 与 `miyu-git` 互相冲突**，本机只能装其一。
- **构建目录半截产物**：中途被 kill 过的 release 构建会留半截 rusqlite 产物报 E0463，
  用 `makepkg -Cf` 从头来（`-C` 会清 src/）。
- **构建内存**：`MemoryMax=12G` 实测够用（峰值 11.2G）；不设上限时一次编译加测试曾把 61G 内存 +
  swap 全吃光拖死整机。
