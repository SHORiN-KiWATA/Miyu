#!/usr/bin/env python3
"""ask_question 走查用的桩 LLM:先提一个问题,拿到回答后再收尾。

请求里出现 ask_question 的工具结果,就说明问题已经解决,回一句短的收尾;
否则流出一个 ask_question 工具调用。

用法:STUB_PORT=18497 python3 stub_llm.py
"""

import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18497"))
CHUNK_SLEEP = float(os.environ.get("STUB_CHUNK_SLEEP", "0.01"))

QUESTION = {
    "questions": [
        {
            "header": "确认",
            "question": "这是一条走查用的问题,选中任意一项即可。",
            "options": [
                {"label": "甲", "description": "第一个选项"},
                {"label": "乙", "description": "第二个选项"},
                {"label": "丙", "description": "第三个选项"},
            ],
        }
    ]
}
ARGUMENTS = json.dumps(QUESTION, ensure_ascii=False)


def answered(body):
    for message in body.get("messages", []):
        if message.get("role") == "tool":
            return True
        content = message.get("content")
        if isinstance(content, list):
            for part in content:
                if isinstance(part, dict) and part.get("type") == "tool_result":
                    return True
    return False


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, payload):
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if answered(body):
            for chunk in ("走查", "结束", "。"):
                self._sse({"choices": [{"index": 0, "delta": {"content": chunk}, "finish_reason": None}]})
                time.sleep(CHUNK_SLEEP)
            self._sse({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                       "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}})
        else:
            self._sse({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "call_ask_1", "type": "function",
                 "function": {"name": "ask_question", "arguments": ARGUMENTS}}]},
                "finish_reason": None}]})
            time.sleep(CHUNK_SLEEP)
            self._sse({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                       "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}})
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
