# 虚拟化：OrbStack、Docker、UTM

Apple Silicon Mac 上跑 Linux 虚拟机、容器和 Windows 的常用方案。

## 方案对比

| 工具 | 类型 | 特点 |
|---|---|---|
| OrbStack | Docker + Linux 轻量虚拟机 | 轻快、省资源，macOS 上跑 Docker 首选；自带 Linux 机器 |
| Docker Desktop | Docker 官方 | 功能全但吃资源，收费政策恶心人 |
| Colima | CLI 版容器运行时 | 轻量无 GUI，配合 docker CLI 用 |
| UTM | 通用虚拟机 | 基于 QEMU，可跑完整 Linux/Windows/macOS 虚拟机 |
| Parallels | 商业虚拟机 | 体验好、性能强，Windows 支持最好，收费 |
| VMware Fusion | 商业虚拟机 | 个人免费版可用，跑 Windows/Linux |

## OrbStack 快速上手

```zsh
brew install --cask orbstack    # 安装
orb list                        # 列出 Linux 机器
orb create ubuntu mybox         # 建一台 Ubuntu 机器
orb run -m mybox                # 进入这台机器的 shell
orbctl start                    # 启动服务
```

装了 OrbStack 后 `docker` 命令直接可用（它自带 Docker 引擎），不用再装 Docker Desktop。

## UTM 跑虚拟机

- 下载 UTM（App Store 或 GitHub），新建虚拟机 → 选「虚拟化」（Apple Silicon 上性能好）。
- 跑 ARM 版系统镜像（如 Ubuntu ARM、Windows ARM）效率高；x86 镜像走模拟会很慢。
- 共享文件夹：虚拟机设置 → 共享目录。

## 常见问题

- **M 芯片跑 x86 容器/镜像**：Docker 会通过 Rosetta 转译，多数能用但慢；优先找 arm64 镜像。
- **端口占用**：容器端口映射冲突时报错，`lsof -iTCP:端口` 查谁占了。
- **磁盘膨胀**：虚拟机/容器磁盘文件只增不减是正常的，定期清理不用的镜像和容器。
- **Docker 权限**：OrbStack 装好一般免配置；报权限错就 `orbctl status` 看服务状态。

## 清理

```zsh
docker system prune -a     # 清理所有未使用的镜像/容器/缓存
orb delete <name>          # 删掉某台 Linux 机器
```

注意：`docker system prune -a` 会把本地没在用的镜像全删，下载要重新拉。
