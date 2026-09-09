#!/usr/bin/env python3
"""桩 LLM:OpenAI 兼容 SSE。用户消息里带 LINES=<n> 就回 n 行正文(分块慢速流出),
否则回一行 ok。用来把 REPL 屏幕填满,复现活动区钉在屏底时的回车提交。"""
import json
import os
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18497"))


def content_text(message):
    content = message.get("content")
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return content or ""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        last = messages[-1] if messages else {}
        last_user = content_text(last) if last.get("role") == "user" else ""
        m = re.search(r"LINES=(\d+)", last_user)
        lines = int(m.group(1)) if m else 0
        delay = float(os.environ.get("STUB_CHUNK_DELAY", "0.03"))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        base = {"id": "stub", "object": "chat.completion.chunk", "model": body.get("model")}

        def sse(payload):
            self.wfile.write(b"data: " + json.dumps(payload, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()

        if lines:
            for i in range(lines):
                sse({**base, "choices": [{"index": 0, "delta": {"role": "assistant",
                     "content": f"line {i + 1} of {lines} — filler text to occupy the row\n"}, "finish_reason": None}]})
                time.sleep(delay)
        else:
            sse({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": None}]})
        sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
             "usage": {"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
