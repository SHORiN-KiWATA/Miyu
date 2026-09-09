#!/usr/bin/env python3
"""桩 LLM：普通请求秒回，**摘要请求故意慢**。

压缩期间别的会话还能不能用，只有让压缩真的慢下来才测得出来。摘要请求靠
系统提示里的 "context summarization assistant" 认（和仓库里其它测具同一个
判据），睡 $STUB_COMPACT_SECS 秒再回。
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18761"))
COMPACT_SECS = float(os.environ.get("STUB_COMPACT_SECS", "25"))


def sse(text):
    chunks = [
        f'data: {json.dumps({"choices": [{"delta": {"content": text}}]})}\n\n',
        'data: {"choices":[{"finish_reason":"stop","delta":{}}]}\n\n',
        'data: {"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10,"total_tokens":110}}\n\n',
        "data: [DONE]\n\n",
    ]
    return "".join(chunks).encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = raw.decode("utf-8", "replace")
        is_compact = "context summarization assistant" in body
        if is_compact:
            time.sleep(COMPACT_SECS)
            text = "## Standing Facts & Constraints\\n- stub summary"
        else:
            text = "ok"
        payload = sse(text)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
