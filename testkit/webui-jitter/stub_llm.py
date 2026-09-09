#!/usr/bin/env python3
"""流式抖动走查用的桩 LLM:回一段**很长**的 markdown,每 10ms 吐一小块。

正文要够长、够杂(列表/代码块/标题/长段落),气泡才会持续往下长好几秒——
Safari 尾部上下鬼畜正是在这段时间里出现的。

用法:STUB_PORT=18496 python3 stub_llm.py
"""

import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18496"))
CHUNK_CHARS = int(os.environ.get("STUB_CHUNK_CHARS", "10"))
CHUNK_SLEEP = float(os.environ.get("STUB_CHUNK_SLEEP", "0.01"))

REPLY_OK = "ok"
REASONING = "用户要一段长回复,那就把 Arch 装机的整个流程讲一遍,顺手带几段命令。"


def build_reply():
    parts = ["先说结论:整个流程分四步,每一步都有能回退的办法。\n\n"]
    for section in range(1, 9):
        parts.append(f"## 第 {section} 步:准备与检查\n\n")
        parts.append(
            "这一步要确认启动介质、分区表和网络。UEFI 与 BIOS 的差别在于引导器放的位置,"
            "前者写进 EFI 分区,后者写进 MBR;混着来就是常见的「装完不启动」。"
            "分区之前先看清楚盘符,`lsblk -f` 列出来的才算数,别凭记忆。\n\n"
        )
        parts.append("- 检查网络:`ping -c 3 archlinux.org`\n")
        parts.append("- 校准时间:`timedatectl set-ntp true`\n")
        parts.append("- 看分区:`lsblk -f`,确认目标盘没有挂载\n")
        parts.append("- 镜像源:`reflector --country Japan --latest 10 --save /etc/pacman.d/mirrorlist`\n\n")
        parts.append("```sh\n")
        parts.append(f"# 第 {section} 步的命令\n")
        parts.append("sgdisk -Z /dev/nvme0n1\n")
        parts.append("sgdisk -n 1:0:+512M -t 1:ef00 /dev/nvme0n1\n")
        parts.append("sgdisk -n 2:0:0 -t 2:8300 /dev/nvme0n1\n")
        parts.append("mkfs.fat -F32 /dev/nvme0n1p1 && mkfs.btrfs /dev/nvme0n1p2\n")
        parts.append("```\n\n")
        parts.append(
            "然后是 pacstrap 基础包。别一次装太多,先 base、linux、linux-firmware,"
            "再 chroot 进去补齐。fstab 用 genfstab 生成之后要打开看一眼,"
            "尤其 btrfs 子卷的 subvol 选项,漏了会挂到根卷上。\n\n"
        )
    parts.append("最后重启前把引导器装好,systemd-boot 一条命令,GRUB 多两步。装完就能进系统了。\n")
    return "".join(parts)


REPLY = build_reply()


def split(text, size=CHUNK_CHARS):
    return [text[i:i + size] for i in range(0, len(text), size)]


def is_main_turn(body):
    for message in body.get("messages", []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = " ".join(part.get("text", "") for part in content if isinstance(part, dict))
        if "JITTER" in str(content or ""):
            return True
    return False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        text = REPLY if is_main_turn(body) else REPLY_OK
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if text is not REPLY_OK:
            for chunk in split(REASONING, 12):
                payload = {"choices": [{"index": 0, "delta": {"reasoning_content": chunk},
                                        "finish_reason": None}]}
                self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
                self.wfile.flush()
                time.sleep(CHUNK_SLEEP)
        for chunk in split(text):
            payload = {"choices": [{"index": 0, "delta": {"content": chunk}, "finish_reason": None}]}
            self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
            self.wfile.flush()
            time.sleep(CHUNK_SLEEP)
        done = {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": len(text) // 2,
                          "total_tokens": 10 + len(text) // 2}}
        self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": []}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
