#!/usr/bin/env python3
"""CLI 体系黑盒测具的桩 LLM:OpenAI 兼容 SSE,把「它看到的请求」当正文回给客户端。

每条回复的正文是一段 JSON,字段:

    model            请求体里的 model(验 --model 是否生效)
    system_head      第一条 system 消息前 60 字符(验 --system-prompt 替换)
    host_instructions system 消息里 <host-instructions> 块的内容,没有则 null(验 --append-system-prompt)
    tools            tools 数组里的函数名(验 --tools / --no-tools)
    user_count       带 TK 标记的 user 消息条数(验会话历史是否延续/清空;人格预设对话不算)
    last_user_len    最后一条 user 消息的字符数(验 --stdin 长输入不截断)

用户消息里的暗号:
    SLOW          回复前睡 $STUB_SLOW_SECS 秒(验 --timeout)
    ASK_QUESTION  先调 ask_question 工具问一句,拿到答案后把答案原样回出来(验 stdio 的 question/answer)
    FAIL          返回 HTTP 500(验 run.failed → error 事件与退出码 1)

标题/整理等旁路请求(没有 user 暗号、或系统提示不是主对话)一律回「ok」。
"""
import json
import os
import re
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18494"))
SLOW_SECS = float(os.environ.get("STUB_SLOW_SECS", "4"))
LOG = os.environ.get("STUB_LOG")


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
        # 人格预设对话也是 user 角色,只数带 TK 标记的真实测试消息。
        users = [m for m in messages if m.get("role") == "user" and "TK" in content_text(m)]
        last_user = content_text(users[-1]) if users else ""
        systems = [content_text(m) for m in messages if m.get("role") == "system"]
        system_text = "\n".join(systems)
        match = re.search(r"<host-instructions>\n(.*?)\n</host-instructions>", system_text, re.S)
        tools = [t.get("function", {}).get("name") for t in body.get("tools") or []]
        summary = {
            "model": body.get("model"),
            "system_head": (systems[0] if systems else "")[:60],
            "host_instructions": match.group(1) if match else None,
            "tools": tools,
            "user_count": len(users),
            "last_user_len": len(last_user),
        }
        last = messages[-1] if messages else {}
        log_line({"t": time.time(), "summary": summary, "last_role": last.get("role")})

        if "FAIL" in last_user and last.get("role") == "user":
            self.send_response(500)
            self.send_header("Content-Type", "application/json")
            self.send_header("Connection", "close")
            payload = json.dumps({"error": {"message": "stub failure"}}).encode()
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        if "SLOW" in last_user:
            time.sleep(SLOW_SECS)

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Connection", "close")
        self.end_headers()
        base = {"id": "stub", "object": "chat.completion.chunk", "model": body.get("model")}

        def sse(payload):
            self.wfile.write(b"data: " + json.dumps(payload, ensure_ascii=False).encode() + b"\n\n")
            self.wfile.flush()

        def text_reply(text):
            half = max(1, len(text) // 2)
            for chunk in (text[:half], text[half:]):
                sse({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": chunk}, "finish_reason": None}]})
            sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49}})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()

        if "ASK_QUESTION" in last_user and last.get("role") == "user" and "ask_question" in tools:
            sse({**base, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_q1", "type": "function",
                "function": {"name": "ask_question", "arguments": json.dumps({"questions": [{
                    "header": "颜色", "question": "选一个颜色", "options": [
                        {"label": "红", "description": "red"}, {"label": "蓝", "description": "blue"}]}]})},
            }]}, "finish_reason": None}]})
            sse({**base, "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]})
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        if last.get("role") == "tool":
            text_reply("ANSWER:" + content_text(last))
            return
        if last.get("role") != "user":
            text_reply("ok")
            return
        text_reply(json.dumps(summary, ensure_ascii=False))


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
