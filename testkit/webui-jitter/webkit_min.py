#!/usr/bin/env python3
"""最小复现:WebKitGTK 里一个 overflow:auto 的滚动容器,把里面某个块的子节点整体
replaceChildren 掉,scrollTop 会不会被归零。不带 Miyu 的任何 CSS/JS。

    cage -- python3 webkit_min.py [variant]

variant:
    plain      纯 div 嵌套
    zoom       外层加 zoom:1.1(Miyu 的 .app-shell 用它)
    fit        块用 width:fit-content(Miyu 的 .assistant-content.is-slim)
"""

import json
import sys

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("WebKit", "6.0")
from gi.repository import Gio, GLib, Gtk, WebKit  # noqa: E402

VARIANT = sys.argv[1] if len(sys.argv) > 1 else "plain"
SHELL_STYLE = {"plain": "", "zoom": "zoom:1.1;", "fit": ""}.get(VARIANT, "")
BLOCK_STYLE = {"plain": "", "zoom": "", "fit": "width:fit-content;max-width:100%;"}.get(VARIANT, "")
# grid 系列:照抄 Miyu 的 .main-stage > .conversation-stage > .chat-scroll 三层网格。
GRID_LAYOUTS = {
    "grid": """
.shell{height:100vh;display:grid;grid-template-rows:minmax(0,1fr)}
.scroll{min-height:0;overflow:auto}""",
    "nestedgrid": """
.shell{height:100vh;display:grid;grid-template-columns:minmax(0,1fr);grid-template-rows:minmax(0,1fr)}
.stage{display:grid;grid-template-rows:auto minmax(0,1fr);min-height:0}
.scroll{grid-row:2;min-height:0;overflow-y:auto}""",
    "nestedgrid-cq": """
.shell{height:100vh;display:grid;grid-template-columns:minmax(0,1fr);grid-template-rows:minmax(0,1fr);container-type:inline-size}
.stage{display:grid;grid-template-rows:auto minmax(0,1fr);min-height:0}
.scroll{grid-row:2;min-height:0;overflow-y:auto}""",
    "nestedgrid-contain": """
.shell{height:100vh;display:grid;grid-template-columns:minmax(0,1fr);grid-template-rows:minmax(0,1fr)}
.stage{display:grid;grid-template-rows:auto minmax(0,1fr);min-height:0}
.scroll{grid-row:2;min-height:0;overflow-y:auto;contain:size}""",
}
if VARIANT in GRID_LAYOUTS:
    HTML = f"""<!doctype html><html><head><style>
html,body{{margin:0;height:100%}}
{GRID_LAYOUTS[VARIANT]}
p{{margin:0 0 12px}}
</style></head><body><div class="shell"><div class="stage"><div class="bar">bar</div><div class="scroll" id="sc">
<div id="before"><p>before</p></div>
<div class="block" id="block"></div>
</div></div></div></body></html>"""
else:
    HTML = f"""<!doctype html><html><head><style>
html,body{{margin:0;height:100%}}
.shell{{height:100vh;display:flex;flex-direction:column;{SHELL_STYLE}}}
.scroll{{flex:1;min-height:0;overflow:auto}}
.block{{{BLOCK_STYLE}}}
p{{margin:0 0 12px}}
</style></head><body><div class="shell"><div class="scroll" id="sc">
<div id="before"><p>before</p></div>
<div class="block" id="block"></div>
</div></div></body></html>"""

JS = """
(() => {
  const sc = document.getElementById("sc");
  const block = document.getElementById("block");
  const out = [];
  function fill(n) {
    const frag = document.createDocumentFragment();
    for (let i = 0; i < n; i++) { const p = document.createElement("p"); p.textContent = "line " + i; frag.appendChild(p); }
    return frag;
  }
  block.replaceChildren(fill(60));
  sc.scrollTop = sc.scrollHeight;
  out.push(["after-scroll", sc.scrollTop, sc.scrollHeight, sc.clientHeight]);
  block.replaceChildren(fill(70));
  out.push(["after-replace", sc.scrollTop, sc.scrollHeight]);
  sc.scrollTop = sc.scrollHeight;
  const old = [...block.childNodes];
  block.append(fill(80));
  for (const n of old) n.remove();
  out.push(["after-append-remove", sc.scrollTop, sc.scrollHeight]);
  sc.scrollTop = sc.scrollHeight;
  block.appendChild(fill(5));
  out.push(["after-append-only", sc.scrollTop, sc.scrollHeight]);
  sc.scrollTop = sc.scrollHeight;
  block.firstChild.remove();
  out.push(["after-remove-one", sc.scrollTop, sc.scrollHeight]);
  return JSON.stringify(out);
})()
"""


def main():
    app = Gtk.Application(application_id="dev.miyu.webkitmin", flags=Gio.ApplicationFlags.NON_UNIQUE)

    def activate(app):
        window = Gtk.ApplicationWindow(application=app)
        window.set_default_size(800, 500)
        view = WebKit.WebView()
        window.set_child(view)

        def on_load(view, event):
            if event != WebKit.LoadEvent.FINISHED:
                return

            def run():
                def finish(view, result):
                    value = view.evaluate_javascript_finish(result)
                    print(VARIANT, value.to_string(), flush=True)
                    app.quit()

                view.evaluate_javascript(JS, -1, None, None, None, finish)
                return False

            GLib.timeout_add(300, run)

        view.connect("load-changed", on_load)
        view.load_html(HTML, "http://localhost/")
        window.present()

    app.connect("activate", activate)
    app.run([])


if __name__ == "__main__":
    main()
