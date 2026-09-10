#!/usr/bin/env python3
"""过程时间线的真机走查:沙箱 daemon + 工具剧本桩(stub_tools.py)+ Playwright(Chromium)。

    BIN=<miyu 二进制> WEB=<web 目录> python3 testkit/webui-timeline/run.py

web/*.js 与 styles.css 编进二进制,所以 WEB 给的是哪个目录,页面就用哪份前端——
Playwright 拦下 index.html / app.js / styles.css 换成 WEB 里的文件,二进制本身不用重编。

判定项(一轮「思考 → 2 工具 → 说话 → 1 失败工具 → 思考 → 1 工具 → 最终回答」):
  underscore_intraword 正文里 check_os_info 这类词内下划线原样,只有词边界上的 _…_ 是斜体
  queue_inline       跑着的时候再发一条:作为「排队中」用户气泡出现在时间线末尾(直播气泡之后),托盘不出现
  queue_settles      第一轮结束、排队那条轮到之后:占位撤掉,真正的用户消息出现
  peek_left_aligned  放得下的窥视文字贴着时间左对齐,不带 is-overflow
  peek_tail_on_overflow 手机宽度放不下时切到尾部可见(is-overflow,右边缘贴槽)
  status_inline      工具行的耗时/失败图标紧跟文字,不在最右边
  fold_synced        收起动画逐帧看,线的底端不超过裁剪底边;收完线高 0;开合期间线无 transition
  think_peek         思考内容收着时,思考中那一行里滚着正在想的话(尾部对齐)
  peek_stays_after   想完之后收着的那行里窥视文字还在,展开时藏起
  command_dedup      命令签跑完展开只有「参数」「结果」两块(流式输出已藏),收起态没有输出预览气泡
  think_node         思考中的节点仍是原子图标(svg 显示、芯片形态的三个跳动点不显示、宽 16px)
  prep_row           「准备 xx」签在时间线里、无底色,线已长到它
  live_groups        实时:说话把时间线切成两条,第一条 1 思考 2 工具,第二条 1 工具 + 1 思考 + 1 工具
  live_head_hidden   实时运行中的那条没有总结行
  live_collapsed     她一开口,前一条收起、总结行出现,文字形如「Worked for 1.2 s · 2 tools · 1 thought」
  err_marked         失败的那条工具签 is-failure,所在组总结行含「1 err」
  rail_sized         切断后每条时间线的细线有高度(展开态)或缩到 0(收起态)
  persisted_groups   刷新后从 turn.tool_flow 重建,分组数与实时一致,总结行带落库的耗时(Worked for …)
  no_times           用户消息和助手名字旁都没有时间
  toggle_off_on      设置里关掉「过程自动收起」→ 总结行藏起、全部展开;再开 → 收回
  console_clean      全程无 pageerror / console.error
产物:~/.cache/miyu-webui-timeline/{report.json,daemon.log,*.png}
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
OUT = Path(os.environ.get("OUT", "~/.cache/miyu-webui-timeline")).expanduser()
HOME = OUT / "home"
RUNTIME = OUT / "runtime"
PORT = int(os.environ.get("PORT", "18483"))
STUB_PORT = int(os.environ.get("STUB_PORT", "18497"))
BASE = f"http://127.0.0.1:{PORT}"
ENV = dict(os.environ, MIYU_HOME=str(HOME), XDG_RUNTIME_DIR=str(RUNTIME))


def write_config():
    (HOME / "config").mkdir(parents=True, exist_ok=True)
    config = {
        "active_provider": "stub",
        "active_provider_models": [{"provider_id": "stub", "model": "stub-tools"}],
        "providers": [{
            "id": "stub", "display_name": "Stub", "base_url": f"http://127.0.0.1:{STUB_PORT}/v1",
            "protocol": "openai-chat", "api_key": "stub", "models": ["stub-tools"],
            "model_context_window": {"stub-tools": 100000},
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


def serve_local(route):
    url = route.request.url
    name = url.split("?")[0].rsplit("/", 1)[-1] or "index.html"
    local = WEB / name
    if local.exists():
        ctype = {"html": "text/html; charset=utf-8", "js": "application/javascript; charset=utf-8",
                 "css": "text/css; charset=utf-8"}[name.rsplit(".", 1)[-1]]
        route.fulfill(status=200, body=local.read_bytes(), headers={"content-type": ctype, "cache-control": "no-store"})
    else:
        route.continue_()


GROUPS_JS = """() => {
  const last = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop();
  if (!last) return null;
  return {
    live: last.classList.contains('live-assistant'),
    lines: [...last.querySelectorAll('.proc-line')].map((l) => ({
      open: l.classList.contains('is-open'), live: l.classList.contains('is-live'), static: l.classList.contains('is-static'),
      headHidden: l.querySelector('.proc-head').hidden, summary: l.querySelector('.proc-summary')?.textContent || '',
      tools: l.querySelectorAll('.tool-card').length, thoughts: l.querySelectorAll('.reasoning-block').length,
      failures: l.querySelectorAll('.tool-card.is-failure').length,
      railTop: parseFloat(l.querySelector('.proc-rail').style.top || '0'), railHeight: parseFloat(l.querySelector('.proc-rail').style.height || '0'),
    })),
    texts: [...last.querySelectorAll(':scope .assistant-blocks > .markdown-body')].map((m) => m.textContent.slice(0, 40)),
    labelSpans: [...last.querySelectorAll('.assistant-label span')].map((s) => s.textContent),
    userActionSpans: [...document.querySelectorAll('.user-message .message-actions > span')].map((s) => s.textContent),
  };
}"""


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    HOME.mkdir(parents=True)
    RUNTIME.mkdir(parents=True)
    write_config()
    report = {"bin": str(BIN), "web": str(WEB)}
    errors = []
    stub = subprocess.Popen([sys.executable, str(HERE / "stub_tools.py")], env=dict(os.environ, STUB_PORT=str(STUB_PORT)),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    daemon = None
    try:
        assert wait_http(f"http://127.0.0.1:{STUB_PORT}/v1/models"), "stub not up"
        daemon = subprocess.Popen([str(BIN), "__daemon", "--port", str(PORT)], env=ENV, cwd=str(HOME),
                                  stdout=(OUT / "daemon.log").open("w"), stderr=subprocess.STDOUT)
        assert wait_http(f"{BASE}/api/config"), "daemon not up"
        time.sleep(1)
        created = api("POST", "/api/sessions", {"name": "时间线走查", "switch": True})
        session_id = created.get("session_id") or created.get("id") or (created.get("session") or {}).get("session_id")
        assert session_id, f"no session id in {created}"

        with sync_playwright() as pw:
            browser = pw.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 900})
            page.on("pageerror", lambda e: errors.append(f"pageerror: {e}"))
            # 沙箱 home 没有 matugen 主题文件和人格头像,那两处 404 是既有噪声,不算错
            page.on("console", lambda m: errors.append(f"console: {m.text}") if m.type == "error" and "status of 404" not in m.text else None)
            page.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            # 思考内容收着(默认是开的):这样才能验「思考中那一行里滚思考文字」
            page.add_init_script("try { localStorage.setItem('miyu.web.reasoningExpanded', 'false'); } catch (_) {}")
            page.goto(BASE)
            page.wait_for_selector("#composerInput:not([disabled])", timeout=20000)
            page.wait_for_timeout(800)
            # 用户消息里也带一个代码块:用户气泡改中性色后,里面那块深蓝要跟着换
            page.fill("#composerInput", "跑一下时间线剧本\n\n```kdl\nbinds {\n    Mod+Return { spawn \"kitty\"; }\n}\n```")
            page.click("#sendButton")

            # 实时:轮询,抓「第一条切断、第二条运行中」那一刻
            t0 = time.time()
            queued_seen = None
            live_snapshot = None
            shots = 0
            prep_seen = None
            think_seen = None
            peek_seen = None
            head_hidden_seen = False
            head_shown_live = False
            while time.time() - t0 < 60:
                info = page.evaluate(GROUPS_JS)
                # 运行中的那条时间线任何时刻都不该露出总结行(桩快的时候快照未必抓在运行中,所以全程累计)
                for l in (info or {}).get("lines") or []:
                    if l["live"] and l["headHidden"]:
                        head_hidden_seen = True
                    if l["live"] and not l["headHidden"]:
                        head_shown_live = True
                peek_now = page.evaluate("() => { const p = document.querySelector('.live-assistant .proc-steps > .reasoning-block.is-live:not([open]) > summary > .reasoning-peek'); if (!p) return null; const r = p.getBoundingClientRect(); return { display: getComputedStyle(p).display, text: p.textContent, width: r.width }; }")
                if peek_now and peek_now["text"] and (peek_seen is None or len(peek_now["text"]) > len(peek_seen["text"])):
                    peek_seen = peek_now
                    if len(peek_now["text"]) > 12:
                        page.screenshot(path=str(OUT / "00c-live-peek.png"))
                if think_seen is None and page.evaluate("Boolean(document.querySelector('.live-assistant .proc-steps > .reasoning-block.is-live'))"):
                    think_seen = page.evaluate("() => { const i = document.querySelector('.live-assistant .proc-steps > .reasoning-block.is-live > summary > .reasoning-icon'); const svg = i.querySelector('svg'); const dot = i.querySelector('i'); return { svgShown: svg && getComputedStyle(svg).display !== 'none', dotsShown: dot ? getComputedStyle(dot).display !== 'none' : false, width: i.getBoundingClientRect().width }; }")
                    page.screenshot(path=str(OUT / "00-live-thinking.png"))
                if prep_seen is None and page.evaluate("Boolean(document.querySelector('.live-assistant .tool-preparing-tag'))"):
                    prep_seen = page.evaluate("() => { const t = document.querySelector('.live-assistant .tool-preparing-tag'); return { inSteps: t.parentElement.classList.contains('proc-steps'), text: t.textContent, bg: getComputedStyle(t).backgroundColor, railHeight: parseFloat(t.closest('.proc-line').querySelector('.proc-rail').style.height || '0') }; }")
                    page.screenshot(path=str(OUT / "00b-live-preparing.png"))
                if info and info["live"] and shots == 0 and info["lines"] and info["lines"][0]["tools"] >= 1:
                    page.screenshot(path=str(OUT / "01-live-first-group.png")); shots = 1
                if info and info["live"] and len(info["lines"]) >= 2 and info["lines"][1]["tools"] >= 1 and shots == 1:
                    page.wait_for_timeout(500)
                    live_snapshot = page.evaluate(GROUPS_JS)
                    page.screenshot(path=str(OUT / "02-live-second-group.png")); shots = 2
                if info and not info["live"] and info["texts"] and time.time() - t0 > 3:
                    break
                page.wait_for_timeout(250)
            page.wait_for_timeout(1200)
            page.screenshot(path=str(OUT / "03-done.png"))
            # 词内下划线不变斜体:正文里 check_os_info / bilibili_live_stream 原样,只有 _这个才是斜体_ 是 <em>
            report["underscore"] = page.evaluate("""() => {
              const last = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop();
              const body = [...last.querySelectorAll('.assistant-blocks > .markdown-body')].pop();
              const ems = [...body.querySelectorAll('em')].map((e) => e.textContent);
              return { ems, text: body.textContent };
            }""")
            u = report["underscore"]
            report["underscore_intraword"] = u["ems"] == ["这个才是斜体"] and "check_os_info" in u["text"] and "bilibili_live_stream" in u["text"]
            # ── 排队一幕:再发一条让回复流式,流式中再发一条,它该作为「排队中」气泡挂在末尾 ──
            # (排队消息是步间送达的,在工具回合里发会把过程切成两段,所以单独一幕,不搅和上面的判定)
            page.fill("#composerInput", "再来一条")
            page.click("#sendButton")
            deadline = time.time() + 8
            while time.time() < deadline and not page.evaluate("Boolean(document.querySelector('.live-assistant'))"):
                page.wait_for_timeout(100)
            page.fill("#composerInput", "排队的第二条")
            page.click("#sendButton")
            page.wait_for_timeout(500)
            queued_seen = page.evaluate("""() => {
              const q = document.querySelector('.user-message.is-queued'); if (!q) return null;
              const tl = document.getElementById('timeline');
              return { isLast: tl.lastElementChild === q, badge: q.querySelector('.queue-badge')?.textContent, text: q.querySelector('.user-bubble')?.textContent,
                       afterLive: Boolean(q.previousElementSibling?.classList.contains('live-assistant')), tray: document.getElementById('queueTray')?.hidden };
            }""")
            page.screenshot(path=str(OUT / "03b-queued.png"))
            deadline = time.time() + 40
            queue_consumed = None
            while time.time() < deadline:
                queue_consumed = page.evaluate("""() => {
                  const tl = document.getElementById('timeline');
                  const real = [...tl.querySelectorAll('.user-message:not(.is-queued) .user-bubble')].some((b) => b.textContent.includes('排队的第二条'));
                  return { placeholders: tl.querySelectorAll('.user-message.is-queued').length, real, live: Boolean(document.querySelector('.live-assistant')) };
                }""")
                if queue_consumed["real"] and queue_consumed["placeholders"] == 0 and not queue_consumed["live"]:
                    break
                page.wait_for_timeout(300)
            page.wait_for_timeout(800)
            page.screenshot(path=str(OUT / "03c-queue-consumed.png"))
            report["queued_seen"] = queued_seen
            report["queue_consumed"] = queue_consumed
            report["queue_inline"] = bool(queued_seen) and queued_seen["isLast"] and queued_seen["afterLive"] and "排队中" in (queued_seen["badge"] or "") and "排队的第二条" in (queued_seen["text"] or "") and queued_seen["tray"] is True
            report["queue_settles"] = bool(queue_consumed) and queue_consumed["real"] and queue_consumed["placeholders"] == 0 and not queue_consumed["live"]
            done = page.evaluate(GROUPS_JS)
            report["live_snapshot"] = live_snapshot
            report["done"] = done
            report["think_seen"] = think_seen
            report["prep_seen"] = prep_seen
            report["peek_seen"] = peek_seen
            # 思考收着时,思考中那一行里要有思考文字在滚(display flex、有文字、占了宽度)
            report["think_peek"] = bool(peek_seen) and peek_seen["display"] == "flex" and len(peek_seen["text"]) > 0 and peek_seen["width"] > 40
            # 想完之后窥视文字留着(收着的时候);展开那条就藏
            report["peek_after"] = page.evaluate("() => { const b = document.querySelector('.assistant-message:has(.proc-line) .proc-steps > .reasoning-block'); if (!b) return null; const p = b.querySelector('.reasoning-peek'); const shown = getComputedStyle(p).display !== 'none' && p.textContent.length > 0; b.open = true; const hiddenWhenOpen = getComputedStyle(p).display === 'none'; b.open = false; return { shown, hiddenWhenOpen }; }")
            report["peek_stays_after"] = bool(report["peek_after"]) and report["peek_after"]["shown"] and report["peek_after"]["hiddenWhenOpen"]
            # 放得下的窥视文字要贴着时间左对齐(不带 is-overflow、左边缘紧贴槽的左边缘);状态位紧跟文字,不在最右边
            report["peek_fit"] = page.evaluate("""() => {
              const b = document.querySelector('.assistant-message:has(.proc-line) .proc-steps > .reasoning-block');
              const slot = b.querySelector('.reasoning-peek'); const text = slot.firstElementChild;
              const s = slot.getBoundingClientRect(), t = text.getBoundingClientRect();
              const card = document.querySelector('.assistant-message:has(.proc-line) .proc-steps > .tool-card.is-success');
              const title = card.querySelector('.tool-title').getBoundingClientRect(); const status = card.querySelector('.tool-status').getBoundingClientRect();
              return { overflow: slot.classList.contains('is-overflow'), leftGap: t.left - s.left, fits: t.width <= s.width + 1, statusGap: status.left - title.right, rowRight: card.getBoundingClientRect().right - status.right };
            }""")
            pf = report["peek_fit"]
            report["peek_left_aligned"] = pf["fits"] and not pf["overflow"] and pf["leftGap"] < 2
            report["status_inline"] = pf["statusGap"] < 16 and pf["rowRight"] > 80
            # 思考中的节点是原子图标(svg 显示、跳动点隐藏、宽 16px);「准备」签在时间线里、无底色、线长到了它
            report["think_node"] = bool(think_seen) and think_seen["svgShown"] and not think_seen["dotsShown"] and 15 <= think_seen["width"] <= 19
            report["prep_row"] = bool(prep_seen) and prep_seen["inSteps"] and prep_seen["bg"] in ("rgba(0, 0, 0, 0)", "transparent") and prep_seen["railHeight"] > 20
            ls = (live_snapshot or {}).get("lines") or []
            report["live_head_hidden"] = head_hidden_seen and not head_shown_live
            report["live_collapsed"] = bool(ls) and (not ls[0]["open"]) and (not ls[0]["headHidden"]) and ls[0]["summary"].startswith("Worked for") and "2 tools" in ls[0]["summary"] and "1 thought" in ls[0]["summary"]
            dl = (done or {}).get("lines") or []
            report["live_groups"] = len(dl) == 2 and dl[0]["tools"] == 2 and dl[0]["thoughts"] == 1 and dl[1]["tools"] == 2 and dl[1]["thoughts"] == 1
            report["err_marked"] = len(dl) == 2 and dl[1]["failures"] == 1 and "1 err" in dl[1]["summary"]
            report["rail_sized"] = all((not l["open"] and l["railHeight"] == 0) or (l["open"] and l["railHeight"] > 20) for l in dl)
            report["no_times"] = done is not None and not any(s.strip() for s in done["labelSpans"]) and not done["userActionSpans"]

            # 展开第二条 + 点开失败那行的详情
            page.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')[1]; l.querySelector('.proc-head').click(); }")
            page.wait_for_timeout(600)
            page.evaluate("() => { const last = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop(); (last.querySelector('.tool-card.is-failure .tool-head') || last.querySelector('.proc-line:last-of-type .tool-card .tool-head'))?.click(); }")
            # 同时点开最后那条命令签:展开面板里应只有参数和结果,没有第二份流式输出
            page.evaluate("() => { const cards = (() => { const ls = [...[...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')]; return ls[ls.length - 1].querySelectorAll('.tool-card'); })(); cards[cards.length - 1]?.querySelector('.tool-head')?.click(); }")
            page.wait_for_timeout(600)
            report["command_panel"] = page.evaluate("() => { const cards = (() => { const ls = [...[...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')]; return ls[ls.length - 1].querySelectorAll('.tool-card'); })(); const c = cards[cards.length - 1]; return { visibleDetails: [...c.querySelectorAll('.tool-detail')].filter(d => !d.hidden).map(d => d.querySelector('.tool-detail-label')?.textContent), preview: c.querySelector('.tool-command-output-preview') ? getComputedStyle(c.querySelector('.tool-command-output-preview')).display : null }; }")
            page.screenshot(path=str(OUT / "04-expanded.png"))
            report["rail_after_expand"] = page.evaluate(GROUPS_JS)["lines"][1]["railHeight"]
            # 收起动画期间逐帧看:线的底端不能超过 wrap 当前的裁剪底边(内容收到哪线就到哪)
            page.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')[1]; l.querySelector('.proc-head').click(); }")
            samples = []
            for _ in range(8):
                page.wait_for_timeout(45)
                samples.append(page.evaluate("""() => {
                  const l = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')[1];
                  const box = l.getBoundingClientRect(); const zoom = l.offsetWidth ? box.width / l.offsetWidth : 1;
                  const rail = l.querySelector('.proc-rail'); const clip = l.querySelector('.proc-wrap > div').getBoundingClientRect();
                  const railBottom = parseFloat(rail.style.top || '0') + parseFloat(rail.style.height || '0');
                  return { railBottom, clipBottom: (clip.bottom - box.top) / zoom, transition: getComputedStyle(rail).transitionDuration };
                }"""))
            page.wait_for_timeout(500)
            final_rail = page.evaluate(GROUPS_JS)["lines"][1]["railHeight"]
            report["fold_samples"] = samples
            # 逐帧同步;采样落在帧与帧之间时允许一帧的误差,超过 20px 或多数帧对不上才算失败
            close = sum(1 for s in samples if s["railBottom"] <= s["clipBottom"] + 1.5)
            report["fold_synced"] = close >= len(samples) - 1 and all(s["railBottom"] <= s["clipBottom"] + 20 for s in samples) and final_rail == 0 and any(s["transition"] == "0s" for s in samples)

            # 刷新:回看那份
            page.reload()
            page.wait_for_selector("#composerInput", timeout=20000)
            page.wait_for_timeout(2000)
            page.screenshot(path=str(OUT / "05-reloaded.png"))
            again = page.evaluate(GROUPS_JS)
            report["reloaded"] = again
            al = (again or {}).get("lines") or []
            # 回看的总结行也要有耗时(落库的 started_ms/finished_ms),形如「Worked for 1.2 s · 2 tools · 1 thought」
            report["persisted_groups"] = len(al) == 2 and [(l["tools"], l["thoughts"]) for l in al] == [(2, 1), (2, 1)] and all(l["static"] and not l["headHidden"] and not l["open"] for l in al) and all(l["summary"].startswith("Worked for") and "2 tools" in l["summary"] for l in al)
            report["persisted_err"] = len(al) == 2 and al[1]["failures"] == 1 and "1 err" in al[1]["summary"]
            # 亮色主题也看一眼(用户日常用亮色)
            page.click("#sidebarThemeButton")
            page.wait_for_timeout(500)
            page.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')[1]; l.querySelector('.proc-head').click(); }")
            page.wait_for_timeout(700)
            page.evaluate("() => { const last = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop(); last.querySelector('.tool-card.is-failure .tool-head')?.click(); }")
            page.wait_for_timeout(600)
            page.screenshot(path=str(OUT / "05b-reloaded-light.png"))
            page.evaluate("document.getElementById('chatScroll').scrollTop = 0")
            page.wait_for_timeout(400)
            page.screenshot(path=str(OUT / "05c-reloaded-light-top.png"))
            page.evaluate("document.getElementById('chatScroll').scrollTop = 1e9")
            page.wait_for_timeout(400)
            page.screenshot(path=str(OUT / "05d-reloaded-light-bottom.png"))
            page.click("#sidebarThemeButton")
            page.wait_for_timeout(400)

            # 设置开关
            page.click("#sidebarSettingsButton")
            page.wait_for_selector("#procCollapseToggle", timeout=10000)
            page.wait_for_timeout(600)
            page.click("#procCollapseToggle")
            page.wait_for_timeout(600)
            off = page.evaluate("[...document.querySelectorAll('.proc-line')].map(l => [l.classList.contains('is-open'), l.querySelector('.proc-head').hidden])")
            page.click("#procCollapseToggle")
            page.wait_for_timeout(600)
            on = page.evaluate("[...document.querySelectorAll('.proc-line')].map(l => [l.classList.contains('is-open'), l.querySelector('.proc-head').hidden])")
            report["toggle_off"] = off
            report["toggle_on"] = on
            report["toggle_off_on"] = all(o == [True, True] for o in off) and all(o == [False, False] for o in on)
            report["prefs"] = api("GET", "/api/ui-prefs")
            page.keyboard.press("Escape")
            page.wait_for_timeout(400)
            # 手机视口再看一眼
            phone = browser.new_page(viewport={"width": 390, "height": 844}, device_scale_factor=2, is_mobile=True, has_touch=True)
            phone.route(lambda u: u.startswith(BASE) and (u.rstrip("/") == BASE or any(k in u for k in ("/app.js", "/styles.css", "/index.html"))), serve_local)
            phone.goto(BASE)
            phone.wait_for_selector("#composerInput", timeout=20000)
            phone.wait_for_timeout(2000)
            phone.evaluate("() => { const l = [...document.querySelectorAll('.assistant-message:has(.proc-line)')].pop().querySelectorAll('.proc-line')[1]; l?.querySelector('.proc-head')?.click(); }")
            phone.wait_for_timeout(700)
            phone.screenshot(path=str(OUT / "06-phone.png"))
            # 手机宽度下同一段思考放不下:要切到尾部可见(is-overflow、文字右边缘贴槽的右边缘)
            report["peek_overflow"] = phone.evaluate("""() => {
              const b = [...document.querySelectorAll('.proc-steps > .reasoning-block:not([open])')].pop();
              if (!b) return null;
              const slot = b.querySelector('.reasoning-peek'); const text = slot.firstElementChild;
              const s = slot.getBoundingClientRect(), t = text.getBoundingClientRect();
              return { overflow: slot.classList.contains('is-overflow'), rightGap: s.right - t.right, wider: t.width > s.width };
            }""")
            po = report["peek_overflow"]
            report["peek_tail_on_overflow"] = bool(po) and po["wider"] and po["overflow"] and abs(po["rightGap"]) < 2
            browser.close()
    finally:
        if daemon:
            daemon.terminate()
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.kill()
        stub.terminate()
    report["errors"] = errors
    report["console_clean"] = not errors
    cp = report.get("command_panel") or {}
    # 回看重建的签压根没有预览元素(None),实时的签有但必须藏着("none")
    report["command_dedup"] = cp.get("visibleDetails") == ["参数", "结果"] and cp.get("preview") in (None, "none")
    (OUT / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=1), "utf-8")
    keys = ["underscore_intraword", "queue_inline", "queue_settles", "peek_left_aligned", "peek_tail_on_overflow", "status_inline", "fold_synced", "think_peek", "peek_stays_after", "command_dedup", "think_node", "prep_row", "live_groups", "live_head_hidden", "live_collapsed", "err_marked", "rail_sized", "persisted_groups", "persisted_err", "no_times", "toggle_off_on", "console_clean"]
    for k in keys:
        print(f"{'  ok ' if report.get(k) else 'FAIL '} {k}")
    print("report:", OUT / "report.json")
    return 0 if all(report.get(k) for k in keys) else 1


if __name__ == "__main__":
    sys.exit(main())
