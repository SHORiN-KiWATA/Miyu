### 使用 Lutris 安装《炉石传说》

#### 安装 Lutris

Lutris 可用于所有主流 Linux 发行版。请根据您的系统选择以下安装方法：

**Ubuntu/Debian（及其衍生版，如 Linux Mint）** Lutris 提供了官方 PPA 以方便安装：

Bash

```
# 添加 Lutris PPA  
sudo add-apt-repository ppa:lutris-team/lutris  
# 更新软件包列表  
sudo apt update  
# 安装 Lutris  
sudo apt install lutris  
```

**Fedora/RHEL（及其衍生版，如 Nobara）** 使用官方的 COPR 仓库：

Bash

```
# 启用 Lutris COPR  
sudo dnf copr enable lutris/lutris  
# 安装 Lutris  
sudo dnf install lutris  
```

**Arch Linux/Manjaro** Lutris 已包含在官方 Arch 仓库中：

Bash

```
sudo pacman -S lutris  
```

*注：对于 Manjaro，使用相同的命令（Manjaro 会同步 Arch 的仓库）。*



#### 设置 Wine 依赖项

**安装 32 位库** 大多数 Linux 系统是 64 位的，但 Wine 需要 32 位支持。请安装以下软件包：

*Ubuntu/Debian:*

Bash

```
sudo dpkg --add-architecture i386  # 启用 32 位支持  
sudo apt update  
sudo apt install lib32gcc-s1 lib32stdc++6 lib32z1 lib32ncurses6 lib32gomp1  
```

*Fedora:*

Bash

```
sudo dnf install glibc.i686 mesa-libGL.i686 mesa-libGLU.i686 libgcc.i686 libstdc++.i686  
```

*Arch Linux:*

Bash

```
sudo pacman -S lib32-gcc-libs lib32-glibc lib32-mesa  
```

**配置 Lutris Wine 管理器** Lutris 可以自动下载并管理 Wine 版本。设置方法如下：

1. 打开 Lutris。
2. 前往 **首选项 (Preferences) > 运行器 (Runners)**。
3. 在列表中找到 **Wine**，然后点击 **管理版本 (Manage Versions)**。
4. 选择一个 Wine 构建版本, 这里推荐`wine-staging-11.2-x86_64` 然后点击 **安装 (Install)**。

> 这里选择wine-staging-11.2-x86_64这个版本是因为之前是过很多版本都没有办法启动游戏，这个版本是可以的



#### 通过 Lutris 安装《炉石传说》

可以在国服的官网上下载暴雪战网安装包然后使用Lutris来安装。

也可以使用《炉石传说》在 Lutris.net 上有官方脚本来安装。

安装完成之后启动暴雪战网，在暴雪战网内下载炉石传说然后启动



如果使用的是窗口管理器全屏启动游戏可能会出现bug，解决办法是：

在lutris游戏界面点击战网图标，点击下方启动按钮旁边的红酒瓶标志打开"wine设置选项" 在wine设置中把虚拟桌面打开



### 常见问题排查

**问题：《炉石传说》在启动时崩溃**

- **修复 1：** 在战网中禁用硬件加速。打开 战网 > 设置 > 常规 > 取消选中“使用浏览器硬件加速”。
- **修复 2：** 更新您的 GPU 驱动程序。对于 AMD/Intel，使用最新的 Mesa 驱动；对于 NVIDIA，请使用专有驱动。

**问题：黑屏或图形故障**

- **修复 1：** 降低游戏内画质设置。在《炉石传说》中，前往 **选项 > 画面**，将“画质”设置为“低”或“中”。
- **修复 2：** 禁用 DXVK。在 Lutris 的 **配置 > 系统选项** 中，取消选中“启用 DXVK”（这将改用 OpenGL——性能会稍差一些，但在某些 GPU 上更稳定）。

