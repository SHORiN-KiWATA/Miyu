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

import syntax

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18411"
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/miyu-webui-links")
OUT.mkdir(parents=True, exist_ok=True)

problems = []

# run.py 发的那条用户消息里的 sh 代码块,一字不差。
USER_CODE = (
    "# 重建向量索引\n"
    'export MIYU_HOME="/tmp/mx"\n'
    'miyu kb embed reindex --quiet && echo "done $?"'
)


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
    // 「标题 (地址)」整行成链:标题和地址要在同一个 <a> 里。
    titled: Array.from(body.querySelectorAll("a.title-link")).map((a) => ({
      href: a.href,
      title: a.querySelector(".link-title")?.textContent || "",
      url: a.querySelector(".link-url")?.textContent || "",
    })),
    // file:// 链接:浏览器不让 http 页面跳过去,所以是「点一下复制路径」。
    paths: Array.from(body.querySelectorAll("a.path-link")).map((a) => ({
      href: a.href,
      text: a.textContent,
      cursor: getComputedStyle(a).cursor,
      tip: a.title,
    })),
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

USER_BUBBLE_PROBE = """
() => {
  const bubble = document.querySelector(".user-message .user-bubble");
  if (!bubble) return { codeBlocks: -1, hasFence: true, inlineCodes: 0, links: [], codeText: "" };
  return {
    codeBlocks: bubble.querySelectorAll(".code-block").length,
    hasFence: bubble.textContent.includes("```"),
    inlineCodes: bubble.querySelectorAll("p > code").length,
    links: Array.from(bubble.querySelectorAll("a")).map((a) => a.href),
    codeText: bubble.querySelector(".code-block pre code")?.textContent || "",
  };
}
"""

ATTACHMENT_ICON_PROBE = """
() => {
  const out = {};
  for (const chip of document.querySelectorAll(".user-attachment-file")) {
    const name = chip.querySelector("strong")?.textContent || "?";
    out[name] = chip.querySelector(".icon-slot")?.dataset.icon || "?";
  }
  return out;
}
"""

CARD_HINT_PROBE = """
() => {
  const host = document.createElement('div');
  host.id = 'cardProbe';
  host.style.cssText = 'position:fixed;left:0;top:0;width:200px;z-index:9999';
  host.innerHTML = '<div class="dash-cards"><div class="dash-card">' +
    '<span class="dash-card-label">重建</span>' +
    '<strong class="dash-card-value">2%</strong>' +
    '<span class="dash-card-hint">195/6707 个文件 · ' +
    'How-to_verify_GPG_key_of_official_.ISO_images_en.md</span></div></div>';
  document.body.appendChild(host);
  const card = host.querySelector('.dash-card');
  const hint = host.querySelector('.dash-card-hint');
  const cardBox = card.getBoundingClientRect();
  const hintBox = hint.getBoundingClientRect();
  // 量的是**文字**有没有超出盒子:文字溢出不会撑大元素的 border box,
  // 比 rect 的右边缘永远看不出问题(第一版探针就这么白测了一轮)。
  const out = { overflow: hint.scrollWidth - hint.clientWidth,
                card: Math.round(cardBox.width), hint: hint.scrollWidth };
  host.remove();
  return out;
}
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
        # 控制台的 error 也算数:高亮是在渲染主路径上跑的,它抛异常未必炸到
        # pageerror(catch 住了也算白干),但一定会在控制台留痕。
        # 「Failed to load resource」这类是资源 404 的回声,下面那条 response
        # 钩子已经按 URL 判过了(/theme.css 没配 matugen 主题时必 404,是常态);
        # 在这里再报一遍只会拿到一条没有 URL 的重复噪音。
        page.on("console", lambda message: problems.append(
            f"console.{message.type}: {message.text}")
            if message.type == "error"
            and "Failed to load resource" not in message.text else None)
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
        # 09-09：模型给参考资料写的是纯文本「标题 (地址)」，修之前只有括号里那半
        # 截成链，标题是死字；file:// 更惨，整条 [label](file://…) 原样漏成源码。
        titled = next((item for item in links["titled"]
                       if "arxiv.org" in item["href"]), None)
        check("「标题 (地址)」整行成链", titled is not None,
              json.dumps(links["titled"], ensure_ascii=False))
        if titled:
            check("标题进了同一个链接",
                  titled["title"] == "Efficient LLM Collaboration via Planning",
                  repr(titled["title"]))
            check("地址那半截还在", "2506.11578v3" in titled["url"], titled["url"])
        path_link = next((item for item in links["paths"] if item["href"].startswith("file://")), None)
        check("file:// 链接成链", path_link is not None,
              json.dumps(links["paths"], ensure_ascii=False))
        if path_link:
            check("file:// 链接是「点了复制」的样子", path_link["cursor"] == "copy", path_link["cursor"])
            check("hover 看得到完整路径",
                  path_link["tip"].endswith("/mcp-servers/bilibili-summary"), path_link["tip"])
        check("Markdown 源码没漏出来", "](file://" not in links["text"])
        shot(page, "01-autolink")
        # 参考资料那两行单独拍一张:这两条(标题成链 / file:// 成链)光看结构断言
        # 不知道它长什么样。
        titled_node = page.query_selector("a.title-link")
        if titled_node is not None:
            box = titled_node.evaluate_handle("el => el.closest('ul') || el.parentElement")
            box.as_element().scroll_into_view_if_needed()
            time.sleep(0.3)
            box.as_element().screenshot(path=str(OUT / "08-title-links.png"))
            print("  shot 08-title-links.png")

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

        # ── 自己发的消息：代码块 + 行内代码 + 可点链接 ─────────
        mine = page.evaluate(USER_BUBBLE_PROBE)
        check("自己发的代码块被渲染成代码块", mine["codeBlocks"] == 1,
              f"{mine['codeBlocks']} 个；正文里还剩 ``` 字面量：{mine['hasFence']}")
        check("正文里不再出现 ``` 字面量", mine["hasFence"] is False)
        check("行内代码有独立节点", mine["inlineCodes"] >= 1, str(mine["inlineCodes"]))
        check("自己发的链接可点", mine["links"] == ["https://wiki.archlinux.org/title/Fcitx5"],
              str(mine["links"]))
        check("代码块内容一字未改",
              mine["codeText"] == USER_CODE, repr(mine["codeText"]))
        shot(page, "07-user-message")

        # ── 语法高亮：断言 + 三套配色截图 ───────────────────
        syntax.run(page, check, USER_CODE)

        # ── 附件图标按类型分 ────────────────────────────────
        icons = page.evaluate(ATTACHMENT_ICON_PROBE)
        check("文本附件和视频附件的图标不一样",
              icons.get("todolist.md") != icons.get("clip.mp4"),
              str(icons))
        check("视频附件用视频图标", icons.get("clip.mp4") == "file-video", str(icons))
        check("markdown 附件用 markdown 图标", icons.get("todolist.md") == "file-markdown",
              str(icons))

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

        # ── 统计卡小字不许画出卡片 ──────────────────────────
        # 一个超长文件名就比卡片还宽,默认断行规则不肯在词中间断,整段会画到
        # 边框外面(09-09 用户实拍「重建」卡)。
        spill = page.evaluate(CARD_HINT_PROBE)
        check("统计卡的小字没有画出卡片", spill["overflow"] <= 1,
              f"文字超出 {spill['overflow']}px（盒子 {spill['card']}px / 文字 {spill['hint']}px）")

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

        # ── 外观偏好跨 origin ────────────────────────────────
        # localStorage 按 **origin** 隔离:127.0.0.1 和 localhost 是同一台 daemon
        # 的两个源,跟「换个 IP 进来」是同一回事。09-09 之前主题只存在浏览器本地,
        # 于是换个地址进来就是另一套配色(用户反馈)。
        alt = BASE.replace("127.0.0.1", "localhost")
        page.goto(BASE, wait_until="networkidle")
        page.wait_for_selector("#sidebarThemeButton", timeout=15000)
        before = page.evaluate("() => document.body.dataset.theme")
        page.click("#sidebarThemeButton")
        stored = None
        for _ in range(20):
            time.sleep(0.3)
            stored = page.evaluate(
                "async () => (await (await fetch('/api/ui-prefs',"
                " {credentials: 'same-origin'})).json()).theme")
            if stored:
                break
        after = page.evaluate("() => document.body.dataset.theme")
        check("主题切得动", before != after, f"{before} → {after}")
        check("主题写到了 daemon 那边", stored == after, f"服务端 {stored} / 页面 {after}")
        # 换个源进去,并且**清掉**那个源的本地存储再刷一次:这样通过的话,主题
        # 只可能来自服务端。
        page.goto(alt, wait_until="networkidle")
        page.evaluate("() => localStorage.clear()")
        page.reload(wait_until="networkidle")
        page.wait_for_selector("#sidebarThemeButton", timeout=15000)
        alt_theme = None
        for _ in range(20):
            time.sleep(0.3)
            alt_theme = page.evaluate("() => document.body.dataset.theme")
            if alt_theme == after:
                break
        check("换个 origin 进来主题跟着走", alt_theme == after,
              f"{alt} 上是 {alt_theme}，应为 {after}")

        browser.close()

    print(f"\n{len(problems)} 项没过：" if problems else "\n全过")
    for problem in problems:
        print(f"  - {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
