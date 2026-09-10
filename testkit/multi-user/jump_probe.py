#!/usr/bin/env python3
"""WebUI 流式输出「字会跳」探针:对着一个跑着的 daemon 发一句话,每 60ms 采样
正文气泡的位置/高度/文本长度与时间线 scrollTop,把「往回跳」的时刻记下来并截图。

BASE=http://127.0.0.1:8388 PASSWORD=miyu-sandbox python3 testkit/multi-user/jump_probe.py [提示词]
产物:~/.cache/miyu-jump-probe/{samples.jsonl, jumps.json, jump-*.png, video/}
"""
import json
import os
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = os.environ.get("BASE", "http://127.0.0.1:8388")
PASSWORD = os.environ.get("PASSWORD", "miyu-sandbox")
USERNAME = os.environ.get("USERNAME_", "shorin")
PROMPT = sys.argv[1] if len(sys.argv) > 1 else "用大约四百字介绍一下 Arch Linux 的滚动更新模型,分三段,别用列表。"
OUT = Path("~/.cache/miyu-jump-probe").expanduser()
SECONDS = float(os.environ.get("SECONDS", "45"))

SAMPLE_JS = """
() => {
  const bubbles = document.querySelectorAll('.assistant-message');
  const last = bubbles[bubbles.length - 1];
  // 真正在滚的容器:从气泡往上找第一个 overflow-y 可滚的祖先
  let tl = last ? last.parentElement : document.getElementById('timeline');
  while (tl && tl !== document.body) {
    const o = getComputedStyle(tl).overflowY;
    if ((o === 'auto' || o === 'scroll') && tl.scrollHeight > tl.clientHeight) break;
    tl = tl.parentElement;
  }
  if (!tl || tl === document.body) tl = document.scrollingElement;
  const rect = last ? last.getBoundingClientRect() : null;
  const body = last ? (last.querySelector('.markdown-body') || last) : null;
  const reasoning = last ? last.querySelector('.reasoning, .reasoning-block, details') : null;
  const tlRect = tl.getBoundingClientRect ? tl.getBoundingClientRect() : {top: 0};
  // 最后一个文字块的左下角:横向跳动(markdown 半成品换渲染)看它
  let lastBlock = null;
  if (body) { const kids = body.querySelectorAll('p, li, pre, h1, h2, h3, blockquote'); lastBlock = kids[kids.length - 1] || null; }
  const lb = lastBlock ? lastBlock.getBoundingClientRect() : null;
  return {
    t: performance.now(),
    scroller: tl.id || tl.className || tl.tagName,
    scrollTop: tl.scrollTop,
    scrollHeight: tl.scrollHeight,
    clientHeight: tl.clientHeight,
    docTop: rect ? rect.top - tlRect.top + tl.scrollTop : null,
    lastBlockTop: lb ? lb.top - tlRect.top + tl.scrollTop : null,
    lastBlockH: lb ? lb.height : null,
    lastBlockText: lastBlock ? lastBlock.textContent.length : 0,
    codes: body ? body.querySelectorAll('code').length : 0,
    strongs: body ? body.querySelectorAll('strong, em').length : 0,
    slim: last ? Boolean(last.querySelector('.assistant-content.is-slim')) : null,
    bubbleW: rect ? rect.width : null,
    bubbles: bubbles.length,
    top: rect ? rect.top : null,
    height: rect ? rect.height : null,
    textLen: body ? body.textContent.length : 0,
    live: last ? last.classList.contains('live-assistant') : false,
    reasoningH: reasoning ? reasoning.getBoundingClientRect().height : 0,
    messages: document.querySelectorAll('.message').length,
  };
}
"""


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    for old in OUT.glob("jump-*.png"):
        old.unlink()
    with sync_playwright() as pw:
        browser = pw.chromium.launch()
        mobile = os.environ.get("MOBILE") == "1"
        context = browser.new_context(
            viewport={"width": 390, "height": 844} if mobile else {"width": 1280, "height": 860},
            is_mobile=mobile, has_touch=mobile, device_scale_factor=2 if mobile else 1,
            record_video_dir=str(OUT / "video"))
        page = context.new_page()
        page.goto(BASE)
        page.wait_for_selector("#loginForm:not([hidden]), #composerInput", timeout=20000)
        if page.is_visible("#loginForm"):
            if USERNAME:
                page.fill("#loginUsername", USERNAME)
            page.fill("#loginPassword", PASSWORD)
            page.click("#loginSubmit")
        page.wait_for_selector("#composerInput:not([disabled])", timeout=30000)
        page.wait_for_timeout(800)
        # 新会话,免得老历史干扰(手机布局先拉开会话栏)
        if mobile:
            page.click("#mobileMenuButton")
            page.wait_for_timeout(400)
        page.click("#newChatButton")
        page.wait_for_timeout(600)
        for sel in ["button:has-text('普通')", "[data-mode='normal']", ".mode-chooser button"]:
            if page.query_selector(sel):
                page.click(sel)
                break
        page.wait_for_timeout(800)
        page.fill("#composerInput", PROMPT)
        if mobile:
            page.click("#sendButton")  # 触屏上回车是换行
        else:
            page.keyboard.press("Enter")
        samples = []
        jumps = []
        deadline = time.time() + SECONDS
        prev = None
        shots = 0
        while time.time() < deadline:
            s = page.evaluate(SAMPLE_JS)
            samples.append(s)
            if prev and s["bubbles"] == prev["bubbles"] and s["bubbles"] > 0:
                events = []
                if s["textLen"] < prev["textLen"]:
                    events.append(f"text shrank {prev['textLen']}->{s['textLen']}")
                if s["docTop"] is not None and prev["docTop"] is not None and abs(s["docTop"] - prev["docTop"]) > 2:
                    events.append(f"bubble moved in document {prev['docTop']:.0f}->{s['docTop']:.0f}")
                if s["lastBlockTop"] is not None and prev["lastBlockTop"] is not None and s["lastBlockText"] >= prev["lastBlockText"] and abs(s["lastBlockTop"] - prev["lastBlockTop"]) > 2:
                    events.append(f"last block moved {prev['lastBlockTop']:.0f}->{s['lastBlockTop']:.0f} (text {prev['lastBlockText']}->{s['lastBlockText']})")
                if s["lastBlockH"] is not None and prev["lastBlockH"] is not None and s["lastBlockH"] < prev["lastBlockH"] - 2 and s["lastBlockText"] >= prev["lastBlockText"]:
                    events.append(f"last block shrank {prev['lastBlockH']:.0f}->{s['lastBlockH']:.0f}")
                if s["scrollTop"] is not None and prev["scrollTop"] is not None and s["scrollTop"] < prev["scrollTop"] - 2:
                    events.append(f"scrollTop backwards {prev['scrollTop']}->{s['scrollTop']}")
                if s["height"] is not None and prev["height"] is not None and s["height"] < prev["height"] - 2:
                    events.append(f"bubble height shrank {prev['height']:.0f}->{s['height']:.0f}")
                if s["codes"] != prev["codes"] or s["strongs"] != prev["strongs"]:
                    events.append(f"inline markup flipped code {prev['codes']}->{s['codes']} strong/em {prev['strongs']}->{s['strongs']} (last block h {prev['lastBlockH']}->{s['lastBlockH']})")
                if s["slim"] != prev["slim"] or (s["bubbleW"] and prev["bubbleW"] and abs(s["bubbleW"] - prev["bubbleW"]) > 2):
                    events.append(f"bubble width {prev['bubbleW']}->{s['bubbleW']} slim {prev['slim']}->{s['slim']}")
                if s["reasoningH"] and prev["reasoningH"] and abs(s["reasoningH"] - prev["reasoningH"]) > 2:
                    events.append(f"reasoning height {prev['reasoningH']:.0f}->{s['reasoningH']:.0f}")
                if events:
                    jumps.append({"t": s["t"], "events": events, "prev": prev, "now": s})
                    if shots < 6:
                        page.screenshot(path=str(OUT / f"jump-{shots}.png"))
                        shots += 1
            prev = s
            if samples and not s["live"] and s["textLen"] > 50 and len(samples) > 40:
                # 回合结束再多采 2 秒
                deadline = min(deadline, time.time() + 2)
            page.wait_for_timeout(60)
        (OUT / "samples.jsonl").write_text("\n".join(json.dumps(s) for s in samples))
        (OUT / "jumps.json").write_text(json.dumps(jumps, ensure_ascii=False, indent=1))
        page.screenshot(path=str(OUT / "final.png"))
        context.close()
        browser.close()
    print(f"samples={len(samples)} jumps={len(jumps)} final_text={samples[-1]['textLen'] if samples else 0}")
    for j in jumps[:12]:
        print(f"  t={j['t']:.0f}ms", "; ".join(j["events"]))


if __name__ == "__main__":
    main()
