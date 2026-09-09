#!/usr/bin/env python3
"""生成一份「维基转储」形状的小文件库：大量 .md/.txt，每篇几百字节到几 KB。

用户拖进来的是整份 Manjaro wiki（约 6400 个文件），本地复现不需要真去下
一份，只要文件数量级、大小分布、目录深度对得上就够触发同一条代码路径。
"""

import argparse
import random
from pathlib import Path

TOPICS = [
    "pacman", "systemd", "kernel", "xorg", "wayland", "grub", "btrfs", "nvidia",
    "pipewire", "networkmanager", "bluetooth", "printing", "locale", "fstab",
    "swap", "zram", "firewall", "ssh", "docker", "flatpak",
]

BODY = (
    "{title}\n\n"
    "This page describes how to configure {topic} on the system.\n\n"
    "## Installation\n\n"
    "    sudo pacman -S {topic}\n\n"
    "## Configuration\n\n"
    "Edit the file under /etc/{topic}/ and restart the service.\n"
    "在 /etc/{topic}/ 下修改配置文件，然后重启服务即可生效。\n\n"
    "## Troubleshooting\n\n"
    "{filler}\n"
)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("dest", type=Path)
    parser.add_argument("--count", type=int, default=400)
    parser.add_argument("--seed", type=int, default=7)
    args = parser.parse_args()

    rng = random.Random(args.seed)
    args.dest.mkdir(parents=True, exist_ok=True)
    for index in range(args.count):
        topic = TOPICS[index % len(TOPICS)]
        section = f"{topic}/{index // 50}"
        directory = args.dest / section
        directory.mkdir(parents=True, exist_ok=True)
        # 一部分是极短文件（wiki 里的存根页），用来验证「短到只有一块」也算索引
        if index % 17 == 0:
            text = f"# {topic} stub {index}\n\nSee also {topic}.\n"
        else:
            filler = " ".join(
                rng.choice(
                    [
                        "check the journal with journalctl -xe",
                        "reboot and try again",
                        "the module may be missing from the initramfs",
                        "确认服务已经 enable 并且开机自启",
                        "内核参数写在 /etc/default/grub 里",
                    ]
                )
                for _ in range(rng.randint(6, 60))
            )
            text = BODY.format(title=f"# {topic} guide {index}", topic=topic, filler=filler)
        suffix = ".txt" if index % 11 == 0 else ".md"
        (directory / f"page-{index:05d}{suffix}").write_text(text, encoding="utf-8")
    print(f"seeded {args.count} files under {args.dest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
