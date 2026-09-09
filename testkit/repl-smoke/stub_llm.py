#!/usr/bin/env python3
"""REPL 走查用的桩 LLM:流式吐一小段回复,带真实 usage。

分块是为了让回合层量得出「每秒 token」——单块请求不计入速度。

用法:STUB_PORT=18498 python3 stub_llm.py
"""

import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18498"))
CHUNK_CHARS = int(os.environ.get("STUB_CHUNK_CHARS", "3"))
CHUNK_SLEEP = float(os.environ.get("STUB_CHUNK_SLEEP", "0.02"))
REPLY = "好的,收到。这是一段用于走查的回复,分块吐出来好让 footer 量得出每秒 token。"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, payload):
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        if length:
            self.rfile.read(length)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        for start in range(0, len(REPLY), CHUNK_CHARS):
            self._sse({"choices": [{"index": 0,
                                    "delta": {"content": REPLY[start:start + CHUNK_CHARS]},
                                    "finish_reason": None}]})
            time.sleep(CHUNK_SLEEP)
        completion = max(1, len(REPLY) // 2)
        self._sse({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 12, "completion_tokens": completion,
                             "total_tokens": 12 + completion}})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": [{"id": "stub-model"}]}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
