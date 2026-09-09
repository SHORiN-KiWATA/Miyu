#!/usr/bin/env python3
"""WebKitGTK(和 Safari 同一个引擎)里打开 WebUI、发一条消息、逐帧采样滚动位置。

Playwright 的 webkit 构建在 Arch 上缺 libicu74 跑不起来,系统的 WebKitGTK 2.52
反倒是现成的真 WebKit。要在无头 cage 里跑(见 run.py),不碰用户桌面。

    python3 probe_webkit.py <baseUrl> <outDir>
"""

import json
import os
import sys
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("WebKit", "6.0")
from gi.repository import Gio, GLib, Gtk, WebKit  # noqa: E402

BASE = sys.argv[1]
OUT = Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)
SAMPLER = (Path(__file__).parent / "sampler.js").read_text(encoding="utf-8")
SEND = """
(() => {
  const input = document.getElementById("composerInput");
  if (!input || input.disabled) return "not-ready";
  input.value = "JITTER 来一段很长的装机教程";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  document.getElementById("composerForm").requestSubmit();
  return "sent";
})()
"""
SAMPLE_SECONDS = float(sys.argv[3]) if len(sys.argv) > 3 else 16.0


class Probe:
    def __init__(self, app):
        self.app = app
        self.window = Gtk.ApplicationWindow(application=app)
        self.window.set_default_size(1280, 800)
        self.view = WebKit.WebView()
        self.window.set_child(self.view)
        self.view.connect("load-changed", self.on_load)
        self.view.load_uri(BASE + "/")
        self.window.present()
        self.log = []
        self.tries = 0

    def note(self, text):
        self.log.append(f"{time.strftime('%H:%M:%S')} {text}")
        print(text, flush=True)

    def js(self, script, callback):
        def finish(view, result):
            try:
                value = view.evaluate_javascript_finish(result)
                callback(value.to_string() if value else None)
            except Exception as error:  # noqa: BLE001
                callback(f"error:{error}")

        self.view.evaluate_javascript(script, -1, None, None, None, finish)

    def on_load(self, view, event):
        if event == WebKit.LoadEvent.FINISHED:
            self.note("page loaded")
            GLib.timeout_add(500, self.try_send)

    def try_send(self):
        self.tries += 1
        if self.tries > 60:
            self.note("composer never became ready")
            self.finish(None)
            return False

        def after_arm(result):
            self.note(f"sampler {result}")
            prelude = os.environ.get("JIT_PRELUDE", "")
            if prelude:
                self.js(prelude, lambda r: (self.note(f"prelude {r}"), self.js(SEND, after_send)))
            else:
                self.js(SEND, after_send)

        def after_send(result):
            if result == "sent":
                self.note("message sent; sampling")
                GLib.timeout_add(int(SAMPLE_SECONDS * 1000), self.collect)
            else:
                GLib.timeout_add(500, self.try_send)

        self.js("document.getElementById('composerInput') && !document.getElementById('composerInput').disabled ? 'ready' : 'wait'",
                lambda ready: self.js(SAMPLER, after_arm) if ready == "ready" else GLib.timeout_add(500, self.try_send))
        return False

    def collect(self):
        def done(result):
            self.finish(result)

        self.js("(() => { window.__jit.done = true; window.__jit.meta = [...document.querySelectorAll('.assistant-meta')].map(e => e.textContent); return JSON.stringify(window.__jit); })()", done)
        return False

    def finish(self, samples_json):
        samples = json.loads(samples_json) if samples_json and samples_json.startswith("{") else {"samples": [], "events": []}
        (OUT / "webkit-samples.json").write_text(json.dumps(samples), encoding="utf-8")
        (OUT / "webkit-log.txt").write_text("\n".join(self.log) + "\n", encoding="utf-8")
        self.note(f"{len(samples)} samples written")
        self.app.quit()


def main():
    app = Gtk.Application(application_id="dev.miyu.jitterprobe", flags=Gio.ApplicationFlags.NON_UNIQUE)
    app.connect("activate", lambda app: Probe(app))
    app.run([])


if __name__ == "__main__":
    main()
