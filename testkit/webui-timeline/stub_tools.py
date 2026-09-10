#!/usr/bin/env python3
"""OpenAI 兼容桩:一轮「思考 → 两个工具 → 中途说话 → 一个会失败的工具 → 思考 → 一个工具 → 最终回答」。
给过程时间线的走查用:连续的思考/工具要成组、正文要把组切开、失败要标红、总结行要在切断时出现。

按历史里 assistant 带 tool_calls 的条数判断走到第几幕。
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18497"))
DELAY = float(os.environ.get("STUB_CHUNK_SLEEP", "0.12"))


def _tc(index, call_id, name, args):
    return {"tool_calls": [{"index": index, "id": call_id, "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}]}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, delta, sleep=True):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-tools",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()
        if sleep:
            time.sleep(DELAY)

    def _finish(self, reason="stop"):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-tools",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                   "usage": {"prompt_tokens": 40, "completion_tokens": 30, "total_tokens": 70}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def _reason(self, text):
        for piece in text.split("，"):
            self._sse({"content": None, "reasoning_content": piece + "，"})

    def _text(self, text):
        for i in range(0, len(text), 6):
            self._sse({"content": text[i:i + 6]})

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}") if length else {}
        messages = body.get("messages", [])
        acts = sum(1 for m in messages if m.get("role") == "assistant" and m.get("tool_calls"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if acts == 0:
            # 第一幕:思考,然后两个工具
            self._reason("用户想看时间线，先跑两条命令，一条回显，一条列目录，然后再决定下一步。")
            self._sse(_tc(0, "call_a", "run_command", {"command": "echo 第一条", "title": "回显第一条"}))
            # 第二个调用先只给名字、入参慢慢流:这正是「准备 xx」签露面的窗口(批量里第二个起就算)
            self._sse({"tool_calls": [{"index": 1, "id": "call_b", "type": "function", "function": {"name": "run_command", "arguments": ""}}]})
            time.sleep(float(os.environ.get("STUB_PREP_SLEEP", "1.5")))
            self._sse({"tool_calls": [{"index": 1, "function": {"arguments": json.dumps({"command": "ls /", "title": "列根目录"}, ensure_ascii=False)}}]})
            self._finish("tool_calls")
        elif acts == 1:
            # 第二幕:中途说一句(把组切开),再来一个会失败的工具。
            # 注意 run_command 退出码非零不算失败(Miyu 的规则是输出 JSON 的 ok/success 为
            # false 或 "tool error:" 前缀才算),所以用读一个不存在的文件来拿真正的 tool error。
            self._text("两条都跑完了，再读一个不存在的文件会怎样。")
            self._sse(_tc(0, "call_c", "read_file", {"path": "/definitely-not-here.txt"}))
            self._finish("tool_calls")
        elif acts == 2:
            # 第三幕:思考,再一个工具
            self._reason("那个文件不存在，报错是预期的，最后再跑一条确认一下。")
            self._sse(_tc(0, "call_d", "run_command", {"command": "echo 最后一条", "title": "回显最后一条"}))
            self._finish("tool_calls")
        else:
            self._text("好了，四步跑完：两条回显正常，列根目录正常，读那个不存在的文件如预期报错。")
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
