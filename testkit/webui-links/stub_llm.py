#!/usr/bin/env python3
"""WebUI 走查用的桩 LLM：不花额度，回一段**内容固定**的正文。

正文里刻意混了四种链接形态，正好覆盖链接自动成链与链接卡片的判定分支：

    · 句子中间的裸链接      → 应该变成可点的 <a>，但**不**升级成卡片
    · 独占一整行的裸链接    → 应该升级成卡片
    · 独占一整行的 md 链接  → 同上
    · <https://…> 尖括号形式 → 成链
    · 行内代码里的地址      → 一个字都不许动

末尾三个代码围栏是给语法高亮准备的：认识的语言（rust/diff）要上色，不认识的
（zzunknownlang）与没标语言的那个要原样退回纯文本。

用法：STUB_PORT=18495 python3 stub_llm.py
"""

import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "18495"))

REPLY_OK = "ok"
REASONING = "先想一下用户要什么。他要的是几个常用网站的链接，随手挑几个就好。"

REPLY = """随便挑几个你常用的：Arch Wiki https://wiki.archlinux.org、AUR https://aur.archlinux.org、还有 https://www.protondb.com 这个。

尖括号那种写法也认：<https://archlinux.org>

下面这行是独占一整行的裸链接：

https://example.com

这行是独占一整行的 md 链接：

[GitHub 上的 Miyu](https://github.com/SHORiN-KiWATA/Miyu)

视频站也该出卡：

https://www.youtube.com/watch?v=dQw4w9WgXcQ

第四条独占整行的链接，名额用完之后它应该保持纯链接：

https://zh.wikipedia.org/wiki/Arch_Linux

行内代码里的地址不该被动：`https://example.com/inside-code`，句尾标点也不该被吃进去：见 https://example.org/trailing。

```
https://example.com/inside-fence
```

再看一段代码,这块是给语法高亮用的:

```rust
use std::collections::HashMap;

/// 统计一下
pub fn main() {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    counts.insert("miyu", 1);
    println!("{:?} {}", counts, true);
}
```

补丁也该看得出增删:

```diff
--- a/web/app.js
+++ b/web/app.js
@@ -1,3 +1,4 @@
-  code.textContent = codeText;
+  MiyuHighlight.paint(code, language, codeText, true);
   pre.appendChild(code);
```

不认识的语言必须原样退回纯文本,不能炸:

```zzunknownlang
this is not a real language <b>x</b> & "quoted"
```
"""


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        text = REPLY if is_main_turn(body) else REPLY_OK
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        # 先吐一段思考:WebUI 的「已思考」芯片只有真流过 reasoning 才会出现,
        # 而它被压扁那个 bug 只在真实 DOM 里复现得出来(合成标记复现不了)。
        if text is not REPLY_OK:
            for chunk in split(REASONING, 24):
                payload = {"choices": [{"index": 0,
                                        "delta": {"reasoning_content": chunk},
                                        "finish_reason": None}]}
                self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
                self.wfile.flush()
                time.sleep(0.01)
        for chunk in split(text):
            payload = {
                "choices": [{"index": 0, "delta": {"content": chunk}, "finish_reason": None}]
            }
            self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.01)
        done = {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}}
        self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def do_GET(self):
        # /models 之类的探测一律回空表，daemon 不会因此卡住。
        self.send_response(200)
        self.send_header("content-type", "application/json")
        payload = json.dumps({"data": []}).encode()
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def is_main_turn(body):
    """标题/整理这类旁路请求只回 ok，免得它们也吐一堆链接进历史。"""
    for message in body.get("messages", []):
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if isinstance(content, list):
            content = "".join(p.get("text", "") for p in content if isinstance(p, dict))
        if "LINKTEST" in (content or ""):
            return True
    return False


def split(text, size=40):
    return [text[index:index + size] for index in range(0, len(text), size)]


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
