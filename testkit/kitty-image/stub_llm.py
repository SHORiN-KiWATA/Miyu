#!/usr/bin/env python3
"""kitty 残影专项的桩 LLM:OpenAI 兼容 SSE,固定剧本「先打图,再慢慢吐正文」。

按同一会话里已经出现过几次工具调用来走剧本(每次请求都带完整历史):

    0 次 → 调 load_tools 把 images 组装上(print_image 不是常驻工具)
    1 次 → 调 print_image 打 $STUB_IMAGE(size 由 $STUB_IMAGE_SIZE 指定)
    2 次 → 逐行慢速流式输出 $STUB_LINES 行正文,每行隔 $STUB_DELAY_MS 毫秒
    其余(标题/整理之类的旁路请求)→ 「好的。」

每个请求在 $STUB_LOG 记一行 jsonl(阶段 + 时间戳),e2e 脚本靠它掐时机:
正文开始流式之后再把视口滚上去,流完再截图。
"""
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18493"))
LINES = int(os.environ.get("STUB_LINES", "40"))
DELAY_MS = int(os.environ.get("STUB_DELAY_MS", "60"))
IMAGE = os.environ.get("STUB_IMAGE", "/tmp/blue.png")
IMAGE_SIZE = os.environ.get("STUB_IMAGE_SIZE", "24x4")
LOG = os.environ.get("STUB_LOG", "stub-requests.jsonl")
_lock = threading.Lock()


def log_line(obj):
    with _lock:
        with open(LOG, "a", encoding="utf-8") as f:
            f.write(json.dumps(obj, ensure_ascii=False) + "\n")


def tool_calls_so_far(messages):
    return sum(1 for m in messages if m.get("role") == "assistant" and m.get("tool_calls"))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        last = messages[-1] if messages else {}
        stage = "aside"
        calls = tool_calls_so_far(messages)
        if last.get("role") in ("user", "tool") and any(
            m.get("role") == "user" and "STUB_SCRIPT" in str(m.get("content", "")) for m in messages
        ):
            stage = ["load_tools", "print_image", "text"][min(calls, 2)]
        log_line({"event": "start", "stage": stage, "t": time.time(), "last_role": last.get("role")})
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()

        def sse(payload):
            self.wfile.write(b"data: " + json.dumps(payload, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()

        base = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-model"}

        def tool_call(name, arguments):
            sse({
                **base,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "tool_calls": [{
                            "index": 0,
                            "id": f"call_{name}",
                            "type": "function",
                            "function": {"name": name, "arguments": json.dumps(arguments)},
                        }],
                    },
                    "finish_reason": None,
                }],
            })
            sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})

        try:
            if stage == "load_tools":
                tool_call("load_tools", {"names": ["group:images"]})
            elif stage == "print_image":
                tool_call("print_image", {"image": IMAGE, "size": IMAGE_SIZE})
            elif stage == "text":
                for i in range(LINES):
                    sse({
                        **base,
                        "choices": [{
                            "index": 0,
                            "delta": {"content": f"第 {i + 1} 行正文,图片打完之后继续往下说。\n"},
                            "finish_reason": None,
                        }],
                    })
                    time.sleep(DELAY_MS / 1000)
                sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            else:
                sse({**base, "choices": [{"index": 0, "delta": {"content": "好的。"}, "finish_reason": None}]})
                sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        except BrokenPipeError:
            pass
        log_line({"event": "end", "stage": stage, "t": time.time()})


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
    server.serve_forever()
