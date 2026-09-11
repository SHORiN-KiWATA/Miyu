#!/usr/bin/env python3
"""subagent 测具的桩 LLM：把每次请求的「工具面 + 系统提示词」记下来，并驱动一轮工具循环。

取证点就是子代理那一次请求里模型实际看到的东西：

    system_head   第一条 system 消息的前 80 字符(dev 走 dev-prompt，普通走 subagent-general)
    system_full   完整 system(验 <host-environment> / <runtime cwd=> / 交付约定)
    tools         tools 数组里的函数名(验 dev 是 core_only 那张面)

驱动方式：子代理第一轮回一个 recall_memories 调用(探针：dev 面里不该有这件
工具)，拿到结果后回 run_command(pwd)，再拿到结果就给最终结论。
"""
import json
import os
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18496"))
LOG = os.environ.get("STUB_LOG", "")


def log_line(obj):
    if not LOG:
        return
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps(obj, ensure_ascii=False) + "\n")


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
        systems = [content_text(m) for m in messages if m.get("role") == "system"]
        tools = [t.get("function", {}).get("name") for t in body.get("tools") or []]
        last = messages[-1] if messages else {}
        system_full = systems[0] if systems else ""
        log_line(
            {
                "t": time.time(),
                "system_head": system_full[:80],
                "system_full": system_full,
                "tools": sorted(name for name in tools if name),
                "last_role": last.get("role"),
                "last_text": content_text(last)[:400],
            }
        )

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        base = {"id": "stub", "object": "chat.completion.chunk", "model": body.get("model")}

        def sse(payload):
            self.wfile.write(b"data: " + json.dumps(payload, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()

        def done():
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

        def text_reply(text):
            sse(
                {
                    **base,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {"role": "assistant", "content": text},
                            "finish_reason": None,
                        }
                    ],
                }
            )
            sse(
                {
                    **base,
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48},
                }
            )
            done()

        def tool_reply(call_id, name, arguments):
            sse(
                {
                    **base,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": call_id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": json.dumps(arguments),
                                        },
                                    }
                                ],
                            },
                            "finish_reason": None,
                        }
                    ],
                }
            )
            sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            done()

        tool_rounds = sum(1 for m in messages if m.get("role") == "tool")
        if last.get("role") == "user":
            # 探针一：记忆工具。dev 面(core_only)里不该存在。
            tool_reply("probe_memory", "recall_memories", {"query": "probe"})
            return
        if last.get("role") == "tool" and tool_rounds == 1:
            tool_reply("probe_pwd", "run_command", {"command": "pwd"})
            return
        if last.get("role") == "tool":
            tail = content_text(last).strip().splitlines()
            text_reply("SUBAGENT-DONE " + (tail[-1] if tail else ""))
            return
        text_reply("ok")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
