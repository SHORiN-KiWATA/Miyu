#!/usr/bin/env python3
"""OpenAI 兼容桩:按 MODE 流 reasoning_content / content。
MODE=seq        先 5 段 reasoning_content,再 12 段 content(DeepSeek 常规)
MODE=interleave reasoning 与 content 逐 token 交替
MODE=plain      只有 content
MODE=empty      每个 content delta 都附带 reasoning_content:""(空串)
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18498"))
MODE = os.environ.get("MODE", "seq")
DELAY = float(os.environ.get("STUB_CHUNK_SLEEP", "0.3"))
REASON = ["用户问", "当前模型", "，", "按人格", "不直接说。"]
TEXT = ["问我的模型的话", "那还是不说，", "这", "有什么好讲的。", "要是", "你问的是 pi", "，", "我翻了你本机", "`", "~/.pi/", "agent/settings.json`", "。"]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, delta):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-model",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()
        time.sleep(DELAY)

    def _finish(self, reason="stop"):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-model",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                   "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}") if length else {}
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if MODE == "job":
            # 后台任务剧本:用户说「起一个」→ 调 run_command(background)→ 工具结果回来
            # 短答 → 任务完成后 daemon 自己起唤醒回合(提示里带任务结果)→ 长答。
            messages = body.get("messages", [])

            def text_of(m):
                c = m.get("content")
                if isinstance(c, list):
                    c = "".join(p.get("text", "") for p in c if isinstance(p, dict))
                return c or ""

            if os.environ.get("STUB_DUMP"):
                with open(os.environ["STUB_DUMP"], "a") as f:
                    f.write("---\n" + "\n".join(f"{m.get('role')}: {text_of(m)[:90]!r} tc={bool(m.get('tool_calls'))}" for m in messages[-6:]) + "\n")
            # 用户消息之后可能还挂着瞬态尾巴(联想记忆/运行时事实),不能只看最后一条;
            # 把最近两条 user 内容拼起来看。
            last = messages[-1] if messages else {}
            # 尾巴消息都是 <runtime …> / <artifact-workspace> 这类标签起头,跳过。
            users = [text_of(m) for m in messages if m.get("role") == "user" and not text_of(m).lstrip().startswith("<")]
            content = users[-1] if users else ""
            if last.get("role") != "tool":
                last = {"role": "user", "content": content}
            has_toolcall_history = any(m.get("role") == "assistant" and m.get("tool_calls") for m in messages)
            if last.get("role") == "tool":
                # 工具结果回来后继续长回复:后台任务(sleep 2)会在这段流式中间完成,
                # 复现「AI 还在输出,后台任务回了信息」的时序。
                lines = int(os.environ.get("STUB_AFTER_TOOL_LINES", "60"))
                self._sse({"content": "好的,已经在后台跑了。\n"})
                for i in range(lines):
                    self._sse({"content": f"第 {i + 1} 行,工具之后继续输出的填充文字。\n"})
                self._finish()
                return
            if "起一个" in content and not has_toolcall_history:
                self._sse({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                           "function": {"name": "run_command",
                                                        "arguments": json.dumps({"command": "sleep 2; echo done", "background": True, "title": "睡两秒"})}}]})
                self._finish("tool_calls")
                return
            if has_toolcall_history and any(k in content for k in ("后台", "任务", "done", "完成", "job")) and "第一行" not in content and "第二行" not in content:
                for i in range(int(os.environ.get("STUB_LINES", "80"))):
                    self._sse({"content": f"第 {i + 1} 行,唤醒回合的长回复,用来撑长页面。\n"})
                self._finish()
                return
            self._sse({"content": "好。"})
            self._finish()
            return
        if MODE == "seq":
            for r in REASON:
                self._sse({"role": "assistant", "content": None, "reasoning_content": r})
            for t in TEXT:
                self._sse({"content": t, "reasoning_content": None})
        elif MODE == "interleave":
            for i, t in enumerate(TEXT):
                self._sse({"content": None, "reasoning_content": REASON[i % len(REASON)]})
                self._sse({"content": t, "reasoning_content": None})
        elif MODE == "empty":
            for r in REASON:
                self._sse({"content": None, "reasoning_content": r})
            for t in TEXT:
                self._sse({"content": t, "reasoning_content": ""})
        elif MODE == "long":
            # 80 行短句,慢速流出:给自动滚动走查用(回合要跑几秒,中途有整段重建)。
            for i in range(int(os.environ.get("STUB_LINES", "80"))):
                self._sse({"content": f"第 {i + 1} 行,用来把页面撑长的填充文字。\n"})
        else:
            for t in TEXT:
                self._sse({"content": t})
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-model",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
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
