#!/usr/bin/env python3
"""OpenAI 兼容桩(成员工具走查用):第一幕按 STUB_CALLS(JSON 数组 [{name, args}])把工具
全叫一遍,第二幕说一句话收尾。给 member_tools_probe.py 用:看成员回合里脚本工具、
print_image、read/edit 的显示名与沙盒表现。"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18497"))
CALLS = json.loads(os.environ.get("STUB_CALLS", "[]"))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, delta):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-tools",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()
        time.sleep(0.02)

    def _finish(self, reason="stop"):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-tools",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                   "usage": {"prompt_tokens": 40, "completion_tokens": 30, "total_tokens": 70}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}") if length else {}
        messages = body.get("messages", [])
        acts = sum(1 for m in messages if m.get("role") == "assistant" and m.get("tool_calls"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if acts == 0 and CALLS:
            for index, call in enumerate(CALLS):
                self._sse({"tool_calls": [{"index": index, "id": f"call_{index}", "type": "function",
                                           "function": {"name": call["name"], "arguments": json.dumps(call.get("args", {}), ensure_ascii=False)}}]})
            self._finish("tool_calls")
        else:
            self._sse({"content": "工具都试过了。"})
            self._finish()

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": [{"id": "stub-tools"}]}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
