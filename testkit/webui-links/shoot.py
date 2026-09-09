#!/usr/bin/env python3
"""WebUI 09-09 四项改动的浏览器侧断言 + 截图。由 run.py 起好 daemon 后调用。

    python3 shoot.py <baseUrl> <outDir>

退出码非 0 = 有断言没过，或页面自己报了错。
"""

import json
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18411"
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/miyu-webui-links")
OUT.mkdir(parents=True, exist_ok=True)

problems = []


def check(name, ok, detail=""):
    print(f"{'  ok ' if ok else 'FAIL '}{name}{f' — {detail}' if detail else ''}")
    if not ok:
        problems.append(f"{name}{f' — {detail}' if detail else ''}")


def shot(page, name):
    time.sleep(0.3)
    page.screenshot(path=str(OUT / f"{name}.png"))
    print(f"  shot {name}.png")


PILL_PROBE = """
() => {
  const host = document.createElement("div");
  host.id = "pillProbe";
  host.style.cssText = "width:320px;position:fixed;left:0;bottom:0;z-index:9999";
  host.innerHTML =
    '<section class="tool-card"><button class="tool-head" type="button">' +
    '<span class="tool-icon"></span><span class="tool-title">' +
    "<strong>加载：MCP Open Watch Cinema / cinema_status、MCP 网易云一起听 / get_current_listening_context、MCP B站视频总结 / get_video_summary</strong>" +
    '<small class="tool-summary"></small></span>' +
    '<span class="tool-status"><span>运行中</span></span></button></section>';
  document.body.appendChild(host);
  const head = host.querySelector(".tool-head");
  const strong = host.querySelector(".tool-title strong");
  const headBox = head.getBoundingClientRect();
  const strongBox = strong.getBoundingClientRect();
  return { spill: Math.round(strongBox.right - headBox.right),
           headWidth: Math.round(headBox.width) };
}
"""

LINK_PROBE = """
() => {
  const body = document.querySelector(".assistant-content .markdown-body");
  return {
    auto: Array.from(body.querySelectorAll("a.auto-link")).map((a) => a.href),
    inCode: body.querySelectorAll("code a").length,
    inPre: body.querySelectorAll("pre a").length,
    nested: body.querySelectorAll("a a").length,
    text: body.textContent,
  };
}
"""

CARD_PROBE = """
() => Array.from(document.querySelectorAll(".link-card")).map((node) => ({
  href: node.href,
  title: node.querySelector(".link-card-title")?.textContent || "",
  site: node.querySelector(".link-card-site")?.textContent || "",
  hasImage: Boolean(node.querySelector(".link-card-media img")),
  // 页面整体带 zoom,rect 是缩放后的像素;要验的是 CSS 上限,读计算值。
  maxWidth: getComputedStyle(node).maxWidth,
  fitsParent: node.getBoundingClientRect().width
    <= node.parentElement.getBoundingClientRect().width + 1,
}))
"""

REASONING_PROBE = """
() => {
  const title = document.querySelector('.assistant-content .reasoning-title');
  if (!title) return null;
  return { width: Math.round(title.getBoundingClientRect().width),
           scrollWidth: title.scrollWidth,
           clipped: title.scrollWidth > Math.ceil(title.getBoundingClientRect().width) + 1,
           text: title.textContent };
}
"""

GRID_PROBE = """
() => Array.from(document.querySelectorAll(".dash-gallery .dash-meme")).map((node) => {
  const thumb = node.querySelector(".dash-meme-thumb");
  const box = thumb?.getBoundingClientRect();
  return {
    name: node.querySelector(".dash-meme-name")?.textContent || "",
    height: Math.round(node.getBoundingClientRect().height),
    thumbRatio: box && box.height ? Number((box.width / box.height).toFixed(2)) : 0,
    declaredRatio: thumb?.style.aspectRatio || "",
  };
})
"""


def main():
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 1280, "height": 900})
        page.on("pageerror", lambda error: problems.append(f"pageerror: {error.message}"))
        # /theme.css 404 是常态(没配 matugen 主题),不算问题;别的 4xx/5xx 要看见。
        page.on("response", lambda response: problems.append(
            f"http {response.status}: {response.url}")
            if response.status >= 400 and "/theme.css" not in response.url else None)

        page.goto(BASE, wait_until="networkidle")
        page.wait_for_selector(".assistant-content .markdown-body", timeout=30000)
        time.sleep(0.8)

        # ── 第四项：裸链接自动成链 ──────────────────────────
        links = page.evaluate(LINK_PROBE)
        check("裸链接成链", len(links["auto"]) >= 5,
              f"{len(links['auto'])} 条：{' '.join(links['auto'])}")
        check("行内代码里的地址没被动", links["inCode"] == 0)
        check("代码块里的地址没被动", links["inPre"] == 0)
        check("没有嵌套 <a>", links["nested"] == 0)
        check("句尾句号没被吃进 href",
              "https://example.org/trailing" in links["auto"], " ".join(links["auto"]))
        check("尖括号写法成链",
              any(href.startswith("https://archlinux.org") for href in links["auto"]))
        check("正文里还看得见原样地址", "https://wiki.archlinux.org" in links["text"])
        shot(page, "01-autolink")

        # ── 第八项：链接卡片（真联网抓 OG，最多等 25 秒）─────
        cards = []
        for _ in range(25):
            cards = page.evaluate(CARD_PROBE)
            if len(cards) >= 2:
                break
            time.sleep(1)
        for _ in range(20):
            if len(cards) >= 3:
                break
            time.sleep(1)
            cards = page.evaluate(CARD_PROBE)
        check("独占整行的链接升级成卡片", len(cards) >= 3,
              f"{len(cards)} 张：{json.dumps(cards, ensure_ascii=False)}")
        check("卡片不超过 3 张", len(cards) <= 3, str(len(cards)))
        # YouTube 的 og 标签在第 70 万字节,固定 256KB 上限时抓不到(09-09 实测)。
        youtube = next((c for c in cards if "youtube.com" in c["href"]), None)
        check("视频站(YouTube)也出卡且带图",
              youtube is not None and youtube["hasImage"],
              json.dumps(youtube, ensure_ascii=False) if youtube else "没有 YouTube 卡")
        # 名额是「真的做出卡片」的数量:第四条应该保持纯链接。
        plain = page.evaluate(
            "() => Array.from(document.querySelectorAll('.assistant-content .markdown-body p'))"
            ".filter(p => p.dataset.linkCard === 'none').length")
        check("名额用完后剩下的链接保持原样", plain >= 1, f"{plain} 段")
        check("卡片宽度受控（CSS 上限 520px）",
              all(card["maxWidth"] == "520px" for card in cards),
              ",".join(card["maxWidth"] for card in cards))
        check("卡片没有超出所在段落", all(card["fitsParent"] for card in cards))
        check("句子中间的行内链接没变成卡片",
              not any("wiki.archlinux.org" in card["href"] for card in cards))
        overflow = page.evaluate(
            "() => { const b = document.querySelector('.assistant-content');"
            " return b.scrollWidth - b.clientWidth; }")
        check("正文没有横向溢出", overflow <= 1, f"{overflow}px")
        shot(page, "02-linkcards")

        # ── 第六项：附件预览 ────────────────────────────────
        chip = page.query_selector(".user-attachment-file.is-previewable")
        if chip is None:
            check("附件芯片可预览", False,
                  "没找到 .user-attachment-file.is-previewable")
        else:
            check("附件芯片可预览", True)
            has_download = page.query_selector(".user-attachment-download") is not None
            check("下载箭头独立成一个按钮", has_download)
            chip.click()
            page.wait_for_selector(".attachment-preview:not([hidden])", timeout=8000)
            time.sleep(0.8)
            text = page.evaluate(
                "() => document.querySelector('.attachment-preview-text')?.textContent || ''")
            check("预览里读到了文件正文", "裸链接成链" in text, text[:80])
            shot(page, "05-attachment-preview")
            page.keyboard.press("Escape")
            time.sleep(0.4)
            check("Esc 关掉预览",
                  page.evaluate("() => !!document.querySelector('.attachment-preview[hidden]')"))

        # 视频附件:以前 kindOf() 把 video/* 归成 binary,芯片就是个下载链接。
        clip = page.query_selector('.user-attachment-file.is-previewable[title*="clip.mp4"]')
        if clip is None:
            check("视频附件芯片可预览", False, "没找到 clip.mp4 的可预览芯片")
        else:
            check("视频附件芯片可预览", True)
            clip.click()
            page.wait_for_selector(".attachment-preview:not([hidden])", timeout=8000)
            time.sleep(0.8)
            media = page.evaluate(
                "() => { const v = document.querySelector('.attachment-preview-video');"
                " return v ? { controls: v.controls, ready: v.readyState,"
                " w: v.videoWidth, h: v.videoHeight } : null; }")
            check("弹出的是播放器不是下载", media is not None, str(media))
            if media:
                check("视频真的解出来了", media["w"] > 0 and media["h"] > 0,
                      f"{media['w']}x{media['h']} readyState={media['ready']}")
            shot(page, "06-video-preview")
            page.keyboard.press("Escape")
            time.sleep(0.3)

        # ── 第七项：芯片长文本截断 ──────────────────────────
        # 这个沙箱没有真工具调用，直接把芯片结构塞进页面量它：要验的是 CSS 在
        # 窄容器里会不会把粗体名画到圆角背景外面。
        pill = page.evaluate(PILL_PROBE)
        check("长名字不再画出芯片背景", pill["spill"] <= 0,
              f"溢出 {pill['spill']}px，芯片宽 {pill['headWidth']}")
        shot(page, "03-tool-pill")
        page.evaluate("() => document.getElementById('pillProbe')?.remove()")

        # 反向那一半:短标题不许被挤没(「已思考」曾被压成「已…」,09-09 用户实拍)。
        # 量的是页面上**真实**那枚芯片——桩模型这一轮真的流了 reasoning。合成
        # 标记复现不出来,别拿它当证据。
        reasoning = page.evaluate(REASONING_PROBE)
        if reasoning is None:
            check("页面上有真实的已思考芯片", False, "桩没流出 reasoning?")
        else:
            check("已思考标题没被挤掉", reasoning["clipped"] is False,
                  f"可见 {reasoning['width']}px / 内容 {reasoning['scrollWidth']}px：{reasoning['text']!r}")

        # ── 第十四项：表情包瀑布流 ──────────────────────────
        try:
            page.click("#sidebarSettingsButton", timeout=4000)
        except Exception:
            pass
        time.sleep(0.4)
        tab = page.query_selector('[data-console-panel="memes"].con-rail-item')
        if tab is None:
            check("找得到表情包面板入口", False)
        else:
            tab.click()
            try:
                page.wait_for_selector(".dash-gallery .dash-meme", timeout=15000)
            except Exception:
                pass
            # 图是 lazy 的:不滚到底,屏幕外那些永远不触发 load,也就拿不到真实
            # 比例。滚一圈再回顶,让整页都量得准。
            for _ in range(6):
                page.mouse.wheel(0, 2000)
                time.sleep(0.35)
            page.mouse.wheel(0, -20000)
            time.sleep(1.4)
            grid = page.evaluate(GRID_PROBE)
            check("表情包渲染出来了", len(grid) >= 3, f"{len(grid)} 张")
            heights = {card["height"] for card in grid}
            check("卡片高度不再被同行最高的那张绑死", len(heights) > 1,
                  f"高度集合 {sorted(heights)}")
            tall = next((c for c in grid if c["name"].startswith("tall")), None)
            wide = next((c for c in grid if c["name"].startswith("wide")), None)
            if tall and wide:
                check("瘦高图比宽图高", tall["height"] > wide["height"],
                      f"{tall['height']} vs {wide['height']}")
            # 声明的比例必须真的画出来:flex 子项的自动最小高度会把 aspect-ratio
            # 顶掉,一张 120×420 的图能撑出 646px 的格子(09-09 实测)。
            for card in grid:
                if not card["declaredRatio"]:
                    continue
                declared = float(card["declaredRatio"].split("/")[0].strip())
                check(f"{card['name']} 的格子按声明比例画",
                      abs(card["thumbRatio"] - declared) < 0.06,
                      f"声明 {declared} 实测 {card['thumbRatio']}")
            shot(page, "04-meme-masonry")

        browser.close()

    print(f"\n{len(problems)} 项没过：" if problems else "\n全过")
    for problem in problems:
        print(f"  - {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
