#!/usr/bin/env python3
"""WebUI Artifact 交付能力探针:沙箱 daemon + 工具剧本桩 + Playwright(Chromium)。

    BIN=<miyu 二进制> WEB=<web 目录> python3 testkit/webui-artifact/run.py

一轮里让她用 artifact 工具写四份探针文件,然后逐个在右侧面板里看,记录**现状**——
这不是回归测试,是给「增强交付能力」立基线的量尺,所以每一项只记事实不判对错。

PROPOSED=1 则在路由层预演「放开脚本」的候选值(不改仓库、不重编译),用来回答两个问题:
交互能不能活、活了之后数据能不能外带。PROPOSED_SANDBOX 可覆盖 sandbox 属性组合。

判定项:
  html_probes   probe.html 里各格探针活没活。前四格是能力(内联 CSS/内联脚本/CDN 脚本/data 图),
                后四格是外带通道(fetch / window.open / form 提交 / top 导航)——这几格必须全 BLOCKED。
                注意 form 那格的自述不可信(submit() 被拦时不抛异常),以 console 里浏览器的话为准。
  csp_header    后端给 html artifact 的 CSP 头原文
  svg_*         probe.svg 能不能预览 / 能不能看源码
  csv_*         probe.csv 有没有表格视图
  md_mermaid    probe.md 里的 mermaid 围栏渲染成图还是代码块
  md_katex      probe.md 里的公式有没有 KaTeX
  src_highlight 源码视图里有没有语法高亮 token
产物:~/.cache/miyu-webui-artifact/{report.json,daemon.log,*.png}
"""
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from playwright.sync_api import sync_playwright

HERE = Path(__file__).resolve().parent
BIN = Path(os.environ["BIN"]).expanduser()
WEB = Path(os.environ.get("WEB", HERE.parent.parent / "web")).resolve()
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-webui-artifact")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18485"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18499"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))

# PROPOSED=1:不改仓库、不重编译,在路由层把「放开脚本」的方案预演一遍——
# app.js 的 sandbox 属性和后端下发的 CSP 都换成候选值,看六格探针分别变成什么。
# 目的是回答两个问题:交互能不能活、活了之后数据能不能外带。
PROPOSED = os.environ.get("PROPOSED") == "1"
PROPOSED_SANDBOX = os.environ.get("PROPOSED_SANDBOX", "allow-scripts allow-modals")
PROPOSED_CSP = (
    "sandbox " + PROPOSED_SANDBOX + "; "
    "default-src 'none'; "
    "script-src 'unsafe-inline' 'unsafe-eval' {origin}; "
    "style-src 'unsafe-inline' {origin}; "
    "font-src data: {origin}; "
    "img-src data: blob: {origin}; "
    "media-src data: blob: {origin}; "
    "connect-src 'none'; "
    "form-action 'none'; "
    "frame-src 'none'; "
    "base-uri 'none'"
    # WEBRTC_BLOCK=1 才加这条:先证明不加时这条通道是开的,再证明加了能封。
    + ("; webrtc 'block'" if os.environ.get("WEBRTC_BLOCK") == "1" else "")
)


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-artifact"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-artifact"],
            "model_context_window": {"stub-artifact": 100000},
        }],
        "memory": {"enabled": False},
    }
    (HOME / "config" / "config.jsonc").write_text(json.dumps(config, ensure_ascii=False, indent=2), "utf-8")


def wait_http(url, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            urllib.request.urlopen(url, timeout=2)
            return True
        except Exception:
            time.sleep(0.3)
    return False


def api(method, path, payload=None):
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else {}


def head_of(url):
    """artifact 的响应头原文——CSP 是不是真下发了,只有后端说了算。"""
    try:
        with urllib.request.urlopen(urllib.request.Request(url), timeout=10) as resp:
            return {k.lower(): v for k, v in resp.headers.items()}
    except Exception as exc:
        return {"error": str(exc)}


def serve_local(route):
    url = route.request.url
    name = url.split("?")[0].rsplit("/", 1)[-1] or "index.html"
    local = WEB / name
    if local.exists():
        ctype = {"html": "text/html; charset=utf-8", "js": "application/javascript; charset=utf-8",
                 "css": "text/css; charset=utf-8"}[name.rsplit(".", 1)[-1]]
        body = local.read_bytes()
        if os.environ.get("NOZOOM") == "1" and name == "styles.css":
            before = body
            body = body.replace(b"zoom: var(--artifact-content-scale);", b"zoom: 1;")
            assert body != before, "styles.css 里没找到 artifact-frame 的 zoom"
        if PROPOSED and name == "app.js":
            before = body
            body = body.replace(b'frame.setAttribute("sandbox", "");',
                                f'frame.setAttribute("sandbox", "{PROPOSED_SANDBOX}");'.encode())
            assert body != before, "app.js 里没找到 sandbox 那行,预演补丁失效"
        route.fulfill(status=200, body=body, headers={"content-type": ctype, "cache-control": "no-store"})
    else:
        route.continue_()


SENT_CSP = []


def serve_artifact_with_proposed_csp(route):
    """把后端下发的 CSP 换成候选值,其余原样透传。实际发出去的那份记下来——
    「指令没生效」和「指令根本没发出去」是两回事,不记就分不清。"""
    response = route.fetch()
    headers = dict(response.headers)
    if "text/html" in headers.get("content-type", ""):
        headers["content-security-policy"] = PROPOSED_CSP.format(origin=BASE)
        SENT_CSP.append(headers["content-security-policy"])
    route.fulfill(response=response, headers=headers)


def serve_vendor_with_pna(route):
    """VENDOR_PNA=1:给 /vendor/ 补 CORS + Private Network Access 头,看能不能把
    不透明源加载本机库这条路打通(Chromium 会为跨地址空间请求强制 preflight)。"""
    allow = {
        "access-control-allow-origin": "*",
        "access-control-allow-private-network": "true",
        "access-control-allow-methods": "GET, OPTIONS",
        "access-control-allow-headers": "*",
        "access-control-max-age": "600",
    }
    if route.request.method == "OPTIONS":
        route.fulfill(status=204, headers=allow)
        return
    response = route.fetch()
    headers = dict(response.headers)
    headers.update(allow)
    route.fulfill(response=response, headers=headers)


def pick_artifact(page, name):
    """从资源菜单里选中某份 artifact。返回 False 表示菜单里根本没有它。"""
    page.click("#artifactTitleButton")
    page.wait_for_timeout(200)
    rows = page.locator(".artifact-resource-menu button[role='menuitem']")
    for index in range(rows.count()):
        if name in rows.nth(index).inner_text():
            rows.nth(index).click()
            page.wait_for_timeout(700)
            return True
    page.keyboard.press("Escape")
    return False


def panel_state(page):
    return page.evaluate("""() => ({
      title: document.getElementById('artifactTitle').textContent,
      typeLabel: document.getElementById('artifactTypeLabel').textContent,
      previewHidden: document.getElementById('artifactPreviewButton').hidden,
      // 按钮自己没 hidden 不代表点得到:它整组的父元素也可能被藏起来
      // (svg 就栽在这上面,数据全绿而截图里根本没有切换器)。
      switchHidden: document.getElementById('artifactPreviewButton').parentElement.hidden,
      copyHidden: document.getElementById('artifactCopyButton').hidden,
      imageActionsHidden: document.getElementById('artifactImageActions').hidden,
      sourceHidden: document.getElementById('artifactSourceButton').hidden,
      previewActive: document.getElementById('artifactPreviewButton').classList.contains('active'),
      sourceActive: document.getElementById('artifactSourceButton').classList.contains('active'),
      viewClasses: [...document.getElementById('artifactView').children].map((n) => n.className),
      failure: document.querySelector('#artifactView .artifact-failure')?.textContent || '',
      frameSandbox: document.querySelector('#artifactView iframe')?.getAttribute('sandbox'),
      frameSrc: document.querySelector('#artifactView iframe')?.getAttribute('src'),
      downloadHref: document.getElementById('artifactDownloadButton').getAttribute('href'),
    })""")


def verdict(report):
    """把散落的探针收成一句「过没过」。分两半:该活的活、该死的死——
    只看前一半就上线,等于把箱子通了电却忘了确认它还是密封的。"""
    probes = report.get("html_probes") or {}
    panels = {name: report.get(f"{name}_panel") or {} for name in ("svg", "csv", "md", "html")}
    checks = [
        ("交互活着 · 内联脚本", "RAN" in str(probes.get("inline_script"))),
        ("交互活着 · 真实鼠标点击", str(probes.get("button_zoom_aware_click")) == "CLICKED"),
        ("交互活着 · ECharts 出图", "RENDERED" in str(probes.get("echarts"))),
        ("交互活着 · 本机 vendor 库", "LOADED" in str(probes.get("local_vendor"))),
        ("箱子密封 · cookie", "SecurityError" in str(probes.get("cookie"))),
        ("箱子密封 · localStorage", "SecurityError" in str(probes.get("local_storage"))),
        ("箱子密封 · 父页面 DOM", "SecurityError" in str(probes.get("parent_dom"))),
        ("外带堵死 · fetch", "BLOCKED" in str(probes.get("fetch"))),
        ("外带堵死 · window.open",
         any(word in str(probes.get("window_open")) for word in ("BLOCKED", "THREW"))),
        ("外带堵死 · top 导航", "BLOCKED" in str(probes.get("top_nav"))),
        ("外带堵死 · CDN 脚本", "NOT LOADED" in str(probes.get("cdn_script"))),
        ("外带堵死 · 外链图片", probes.get("remote_img") == 0),
        ("面板 · svg 切换器点得到", panels["svg"].get("switchHidden") is False),
        ("面板 · svg 走图片预览",
         "artifact-image-stage" in str(panels["svg"].get("viewClasses"))),
        ("面板 · csv 画成表格", (report.get("csv_view") or {}).get("tables") == 1),
        ("面板 · 源码有高亮",
         (report.get("html_source_view") or {}).get("tokenSpans", 0) > 0),
    ]
    failed = [name for name, ok in checks if not ok]
    return {"passed": len(checks) - len(failed), "total": len(checks), "failed": failed}


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    report = {"bin": str(BIN), "web": str(WEB), "mode": "proposed" if PROPOSED else "baseline"}
    console = []
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_artifact.py")],
                            env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon not up"
        time.sleep(1)
        api("POST", "/api/sessions", {"name": "artifact 探针", "switch": True})

        with sync_playwright() as pw:
            # BROWSER=firefox:用户本机日常用的是 Firefox,而 Firefox 不实现 Private
            # Network Access,沙箱与外带的判定未必和 Chromium 一致,两边都要过。
            engine = {"firefox": pw.firefox, "webkit": pw.webkit}.get(
                os.environ.get("BROWSER", "chromium"), pw.chromium)
            browser = engine.launch()
            report["browser"] = f'{os.environ.get("BROWSER", "chromium")} {browser.version}'
            page = browser.new_page(viewport={"width": 1440, "height": 900})
            page.on("pageerror", lambda e: console.append(f"pageerror: {e}"))
            page.on("console", lambda m: console.append(f"{m.type}: {m.text}")
                    if m.type in ("error", "warning") else None)
            page.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(
                k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            if PROPOSED:
                page.route(lambda u: "/api/artifacts/" in u, serve_artifact_with_proposed_csp)
            if os.environ.get("VENDOR_PNA") == "1":
                page.route(lambda u: "/vendor/" in u, serve_vendor_with_pna)
            page.goto(BASE)
            page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
            page.wait_for_timeout(600)
            page.fill("#composerInput", "写四份探针文件到预览工作区")
            page.click("#sendButton")

            # 等回合跑完(artifact 工具做完 + 收尾正文出来)
            deadline = time.time() + 60
            while time.time() < deadline:
                if page.evaluate("Boolean(document.querySelector('#artifactToggleButton:not([hidden])'))"):
                    break
                page.wait_for_timeout(300)
            page.wait_for_timeout(2500)
            page.screenshot(path=str(OUT / "00-conversation.png"))

            if page.evaluate("document.getElementById('artifactWorkspace').hidden"):
                page.click("#artifactToggleButton")
                page.wait_for_timeout(800)

            # --- probe.html ---
            found = pick_artifact(page, "probe.html")
            report["html_found"] = found
            if found:
                page.wait_for_timeout(2500)
                report["html_panel"] = panel_state(page)
                page.screenshot(path=str(OUT / "01-html-preview.png"))
                # 去掉 ?download=1:那个参数只改 Content-Disposition,但头要取 iframe 真正拿到的那份
                report["csp_header"] = head_of(report["html_panel"]["frameSrc"])
                probes = {}
                try:
                    frame = page.frame_locator("#artifactView iframe")
                    for key, selector in (("inline_css", "#css"), ("inline_script", "#js"),
                                          ("cdn_script", "#cdn"), ("fetch", "#fetchbox"),
                                          ("window_open", "#popbox"), ("form_post", "#formbox"),
                                          ("top_nav", "#navbox"), ("cookie", "#cookiebox"),
                                          ("local_storage", "#storebox"), ("parent_dom", "#parentbox"),
                                          ("local_vendor", "#vendorbox"), ("local_vendor_css", "#vendorcssbox"),
                                          ("webrtc", "#rtcbox"), ("echarts", "#echartsbox")):
                        try:
                            probes[key] = frame.locator(selector).inner_text(timeout=3000)
                        except Exception as exc:
                            probes[key] = f"<unreachable: {type(exc).__name__}>"
                    for key, selector in (("remote_img", "#rimg"), ("data_img", "#dimg")):
                        try:
                            probes[key] = frame.locator(selector).evaluate(
                                "img => img.naturalWidth", timeout=3000)
                        except Exception as exc:
                            probes[key] = f"<unreachable: {type(exc).__name__}>"
                    # 两条腿:真实点击(会受 .artifact-frame 的 CSS zoom 影响,坐标可能对不上)
                    # 和直接派发事件(只验 onclick 活没活)。分开记,免得把坐标问题读成功能死。
                    try:
                        frame.locator("#b").click(timeout=3000)
                        page.wait_for_timeout(300)
                        probes["button_real_click"] = frame.locator("#b").inner_text(timeout=3000)
                    except Exception as exc:
                        probes["button_real_click"] = f"<unreachable: {type(exc).__name__}>"
                    try:
                        frame.locator("#b").dispatch_event("click", timeout=3000)
                        page.wait_for_timeout(300)
                        probes["button_dispatch"] = frame.locator("#b").inner_text(timeout=3000)
                    except Exception as exc:
                        probes["button_dispatch"] = f"<unreachable: {type(exc).__name__}>"
                    probes["frame_zoom"] = page.evaluate(
                        "() => getComputedStyle(document.querySelector('#artifactView iframe')).zoom")
                    # 自己按 zoom 换算一次真实屏幕坐标再点:这是「人手点得到吗」的答案,
                    # Playwright 自带的 click 在 iframe 带 CSS zoom 时算不对。
                    try:
                        frame.locator("#b").evaluate("b => { b.textContent = 'click me'; }")
                        box = page.evaluate("""() => {
                          const f = document.querySelector('#artifactView iframe');
                          const r = f.getBoundingClientRect();
                          return { left: r.left, top: r.top, zoom: parseFloat(getComputedStyle(f).zoom) || 1 };
                        }""")
                        inner = frame.locator("#b").evaluate(
                            "b => { const r = b.getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; }")
                        page.mouse.click(box["left"] + inner["x"] * box["zoom"],
                                         box["top"] + inner["y"] * box["zoom"])
                        page.wait_for_timeout(300)
                        probes["button_zoom_aware_click"] = frame.locator("#b").inner_text(timeout=3000)
                    except Exception as exc:
                        probes["button_zoom_aware_click"] = f"<failed: {type(exc).__name__}: {exc}>"
                except Exception as exc:
                    probes["frame"] = f"<no frame: {exc}>"
                report["html_probes"] = probes
                # 源码视图
                page.click("#artifactSourceButton")
                page.wait_for_timeout(600)
                report["html_source_view"] = page.evaluate("""() => ({
                  hasSource: Boolean(document.querySelector('#artifactView .artifact-source')),
                  tokenSpans: document.querySelectorAll('#artifactView .artifact-code [class^=\"tok-\"]').length,
                  lineNumbers: document.querySelectorAll('#artifactView .artifact-line-numbers span').length,
                })""")
                page.screenshot(path=str(OUT / "02-html-source.png"))

            # --- chart.html:她拿本机货架画出来的真东西,验收看的就是这张 ---
            found = pick_artifact(page, "chart.html")
            report["chart_found"] = found
            if found:
                page.wait_for_timeout(3000)
                report["chart_canvases"] = page.frame_locator("#artifactView iframe").locator(
                    "canvas").count()
                page.screenshot(path=str(OUT / "08-echarts.png"))

            # --- probe.svg ---
            found = pick_artifact(page, "probe.svg")
            report["svg_found"] = found
            if found:
                page.wait_for_timeout(1200)
                report["svg_panel"] = panel_state(page)
                page.screenshot(path=str(OUT / "03-svg.png"))

            # --- probe.csv ---
            found = pick_artifact(page, "probe.csv")
            report["csv_found"] = found
            if found:
                page.wait_for_timeout(1200)
                report["csv_panel"] = panel_state(page)
                report["csv_view"] = page.evaluate("""() => ({
                  tables: document.querySelectorAll('#artifactView table').length,
                  hasSource: Boolean(document.querySelector('#artifactView .artifact-source')),
                })""")
                page.screenshot(path=str(OUT / "04-csv.png"))

            # --- probe.md ---
            found = pick_artifact(page, "probe.md")
            report["md_found"] = found
            if found:
                page.wait_for_timeout(1500)
                report["md_panel"] = panel_state(page)
                report["md_view"] = page.evaluate("""() => ({
                  katex: document.querySelectorAll('#artifactView .katex').length,
                  mermaidRendered: document.querySelectorAll('#artifactView svg[id^="mermaid"], #artifactView .mermaid svg').length,
                  codeBlocks: document.querySelectorAll('#artifactView .code-block, #artifactView pre code').length,
                  codeLangs: [...document.querySelectorAll('#artifactView .code-toolbar')].map((n) => n.textContent.trim().slice(0, 24)),
                  highlightTokens: document.querySelectorAll('#artifactView .markdown-body [class^=\"tok-\"]').length,
                  tables: document.querySelectorAll('#artifactView table').length,
                })""")
                page.screenshot(path=str(OUT / "05-md-preview.png"))
                page.click("#artifactSourceButton")
                page.wait_for_timeout(600)
                report["md_source_view"] = page.evaluate("""() => ({
                  tokenSpans: document.querySelectorAll('#artifactView .artifact-code [class^=\"tok-\"]').length,
                  lineNumbers: document.querySelectorAll('#artifactView .artifact-line-numbers span').length,
                })""")
                page.screenshot(path=str(OUT / "06-md-source.png"))

            # 面板整体:全屏、宽度、菜单里有几份
            page.click("#artifactTitleButton")
            page.wait_for_timeout(300)
            report["resource_menu"] = page.evaluate(
                "() => [...document.querySelectorAll('.artifact-resource-menu button[role=\\'menuitem\\']')].map((b) => b.textContent.trim())")
            page.screenshot(path=str(OUT / "07-resource-menu.png"))
            page.keyboard.press("Escape")

            browser.close()
    finally:
        for proc in (daemon, stub):
            if proc:
                proc.terminate()
                try:
                    proc.wait(timeout=5)
                except Exception:
                    proc.kill()
    report["sent_csp"] = SENT_CSP[-1] if SENT_CSP else None
    report["console"] = console[:80]
    report["verdict"] = verdict(report)
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2), "utf-8")
    result = report["verdict"]
    print(json.dumps(report, ensure_ascii=False, indent=2))
    print()
    print(f"=== {result['passed']}/{result['total']} 项达标 · {report.get('browser')} ===")
    for name in result["failed"]:
        print(f"  未达标：{name}")
    print(f"截图在 {OUT}")
    sys.exit(1 if result["failed"] else 0)


if __name__ == "__main__":
    main()
