#!/usr/bin/env python3
"""对比桩:同一张流程图,一份用已经内置的 ECharts 画,一份用还没内置的 Mermaid 画。

用来回答「不加那 3.4MB 行不行」——不是看谁的图好看,是看**模型写得出来吗**。
ECharts 画结构图要自己摆每个节点的坐标,Mermaid 只要写清楚谁指向谁。
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18494"))

SHELL_HEAD = """<!doctype html>
<meta charset="utf-8">
<style>
  body { margin: 0; padding: 16px; font-family: system-ui, sans-serif;
         background: #1b1917; color: #e8e3d9; }
  h1 { font-size: 15px; margin: 0 0 12px; opacity: .8; font-weight: 600; }
</style>
"""

ECHARTS_FLOW = SHELL_HEAD + """<h1>ECharts 画的流程图（节点坐标要自己摆）</h1>
<div id="g" style="width:100%;height:340px"></div>
<script src="/vendor/echarts/echarts.min.js"></script>
<script>
var chart = echarts.init(document.getElementById('g'));
chart.setOption({
  series: [{
    type: 'graph', layout: 'none', symbolSize: 54, roam: false,
    label: { show: true, color: '#1b1917', fontSize: 12 },
    edgeSymbol: ['none', 'arrow'], edgeSymbolSize: 9,
    lineStyle: { color: '#8a8378', width: 1.4, curveness: 0 },
    itemStyle: { color: '#c8a26a' },
    data: [
      { name: '\\u7528\\u6237',   x: 0,   y: 100 },
      { name: 'daemon', x: 130, y: 100 },
      { name: '\\u6a21\\u578b',   x: 260, y: 100 },
      { name: '\\u5de5\\u5177',   x: 390, y: 40  },
      { name: '\\u56de\\u590d',   x: 390, y: 160 }
    ],
    links: [
      { source: '\\u7528\\u6237', target: 'daemon' },
      { source: 'daemon', target: '\\u6a21\\u578b' },
      { source: '\\u6a21\\u578b', target: '\\u5de5\\u5177' },
      { source: '\\u5de5\\u5177', target: '\\u6a21\\u578b' },
      { source: '\\u6a21\\u578b', target: '\\u56de\\u590d' }
    ]
  }]
});
</script>
"""

MERMAID_FLOW = SHELL_HEAD + """<h1>Mermaid 画的同一张图（只写谁指向谁）</h1>
<pre class="mermaid">
graph LR
  U[用户] --> D[daemon]
  D --> M[模型]
  M --> T[工具]
  T --> M
  M --> R[回复]
</pre>
<script src="/vendor/mermaid/mermaid.min.js"></script>
<script>
  mermaid.initialize({ startOnLoad: true, theme: 'dark', securityLevel: 'strict' });
</script>
"""

MERMAID_SEQ = SHELL_HEAD + """<h1>Mermaid 的时序图（ECharts 根本没有这个图种）</h1>
<pre class="mermaid">
sequenceDiagram
  participant U as 用户
  participant D as daemon
  participant M as 模型
  U->>D: 发一句话
  D->>M: 带上上下文
  M-->>D: 要调工具
  D->>D: 跑工具
  D->>M: 回灌结果
  M-->>U: 最终回答
</pre>
<script src="/vendor/mermaid/mermaid.min.js"></script>
<script>
  mermaid.initialize({ startOnLoad: true, theme: 'dark', securityLevel: 'strict' });
</script>
"""


def patch_text():
    parts = ["*** Begin Patch"]
    for name, body in (("flow-echarts.html", ECHARTS_FLOW),
                       ("flow-mermaid.html", MERMAID_FLOW),
                       ("seq-mermaid.html", MERMAID_SEQ)):
        parts.append(f"*** Add File: {name}")
        for line in body.splitlines():
            parts.append("+" + line)
    parts.append("*** End Patch")
    return "\n".join(parts) + "\n"


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, delta):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-artifact",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()
        time.sleep(0.04)

    def _finish(self, reason="stop"):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-artifact",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                   "usage": {"prompt_tokens": 40, "completion_tokens": 30, "total_tokens": 70}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}") if length else {}
        acts = sum(1 for m in body.get("messages", [])
                   if m.get("role") == "assistant" and m.get("tool_calls"))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        if acts == 0:
            self._sse({"tool_calls": [{"index": 0, "id": "call_cmp", "type": "function",
                                       "function": {"name": "artifact",
                                                    "arguments": json.dumps({"patchText": patch_text()},
                                                                            ensure_ascii=False)}}]})
            self._finish("tool_calls")
        else:
            for chunk in ["三张图都画好了。", "左边那张是 ECharts 摆坐标画的，", "另外两张是 Mermaid。"]:
                self._sse({"content": chunk})
            self._finish()

    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": [{"id": "stub-artifact"}]}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
