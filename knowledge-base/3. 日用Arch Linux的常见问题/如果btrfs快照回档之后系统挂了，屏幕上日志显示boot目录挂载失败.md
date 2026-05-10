# 原因

因为你的/boot是esp，没能被btrfs快照一起回档，导致内核文件和系统里的模块文件不匹配。

# 解决办法

进ICU，重新挂载根设备和esp，`arch-chroot`之后`pacman -S`重新安装你使用的内核即可修复。
