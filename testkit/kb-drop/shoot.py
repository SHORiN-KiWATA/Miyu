#!/usr/bin/env python3
"""知识库拖放上传的浏览器侧断言 + 截图。由 run.py 起好 daemon 后调用。

    python3 shoot.py <baseUrl> <outDir>

投放事件是合成的:JS 里 new DataTransfer() 装真的 File 再 new DragEvent(...,
{ dataTransfer }) 派发——不填 DataTransfer 的裸 dispatchEvent 什么都证明不了。
目录投放那一步临时替换 DataTransferItem.prototype.webkitGetAsEntry:合成的
DataTransfer 拿不到真 entry(那是 OS 拖拽才有的),不替换就走不到递归那条路。

退出码非 0 = 有断言没过,或页面报了非预期的错误 / 4xx-5xx。
"""

import json
import sys
import time
from pathlib import Path

from playwright.sync_api import sync_playwright

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:18477"
OUT = Path(sys.argv[2] if len(sys.argv) > 2 else "/tmp/miyu-kb-drop")
OUT.mkdir(parents=True, exist_ok=True)

PANEL = '.con-panel[data-console-panel="kb"]'
problems = []
http_errors = []

# 预期内的非 2xx:嵌入没配置时重建接口必然 400(前端 catch 掉了)、守卫拒绝那一步
# 自己要的 400,以及没配自定义主题时 /theme.css 的 404(与本改动无关的既有行为)。
# 其余 4xx/5xx 都算问题。
EXPECTED_400 = ("/api/dash/kb/reindex", "/api/dash/kb/files")
EXPECTED_404 = ("/theme.css",)


def note_console(message):
    # 每个失败请求浏览器都会再喊一句 "Failed to load resource",与 response
    # 那条通道重复;真正要抓的是页面自己的 JS 报错。
    if message.type == "error" and "Failed to load resource" not in message.text:
        problems.append(f"console: {message.text}")


def check(name, ok, detail=""):
    print(f"{'  ok ' if ok else 'FAIL '}{name}{f' — {detail}' if detail else ''}")
    if not ok:
        problems.append(f"{name}{f' — {detail}' if detail else ''}")


def shot(page, name):
    time.sleep(0.35)
    page.screenshot(path=str(OUT / f"{name}.png"))
    print(f"  shot {name}.png")


DISPATCH = """
({ selector, specs, kind, text }) => {
  const dt = new DataTransfer();
  if (text) dt.setData("text/plain", text);
  for (const spec of specs || []) {
    const body = spec.bytes ? new Uint8Array(spec.bytes)
      : (spec.repeat ? spec.text.repeat(spec.repeat) : spec.text);
    dt.items.add(new File([body], spec.name, { type: spec.type || "text/plain" }));
  }
  const target = document.querySelector(selector);
  const event = new DragEvent(kind, { bubbles: true, cancelable: true, dataTransfer: dt });
  target.dispatchEvent(event);
  return { defaultPrevented: event.defaultPrevented, types: Array.from(dt.types) };
}
"""

AFFORDANCE = """
() => {
  const host = document.querySelector('.dash-drop-host');
  const veil = document.querySelector('.dash-drop-veil');
  return {
    host: Boolean(host),
    dropping: Boolean(host && host.classList.contains('is-dropping')),
    veilVisible: Boolean(veil && !veil.hidden && veil.getBoundingClientRect().height > 0),
    label: veil ? veil.textContent.trim() : '',
    pointerEvents: veil ? getComputedStyle(veil).pointerEvents : '',
  };
}
"""

ROWS = """
() => Array.from(document.querySelectorAll('#dashKbRoot .dash-upload-list li')).map((li) => ({
  name: li.firstElementChild.textContent,
  text: li.lastElementChild.textContent,
  cls: li.lastElementChild.className,
}))
"""

# 假的 FileSystemEntry 树:notes/ 下一个文件,再套三层目录,最深那层应当被层数闸挡下。
FAKE_TREE = """
() => {
  const file = (name, text) => ({
    isFile: true, isDirectory: false, name,
    file: (ok) => ok(new File([text], name, { type: "text/plain" })),
  });
  const dir = (name, children) => ({
    isFile: false, isDirectory: true, name,
    createReader: () => {
      let drained = false;
      return { readEntries: (ok) => { ok(drained ? [] : children); drained = true; } };
    },
  });
  window.__kbTree = dir("notes", [
    file("a.md", "# a\\n"),
    dir("deep1", [
      file("b.md", "# b\\n"),
      dir("deep2", [
        file("c.md", "# c\\n"),
        dir("deep3", [file("toodeep.md", "# too deep\\n")]),
      ]),
    ]),
  ]);
  window.__kbOrigEntry = DataTransferItem.prototype.webkitGetAsEntry;
  DataTransferItem.prototype.webkitGetAsEntry = function () { return window.__kbTree; };
}
"""

RESTORE_TREE = """
() => { if (window.__kbOrigEntry) DataTransferItem.prototype.webkitGetAsEntry = window.__kbOrigEntry; }
"""


def dispatch(page, kind, selector=PANEL, specs=None, text=None):
    return page.evaluate(DISPATCH, {"selector": selector, "kind": kind, "specs": specs, "text": text})


def drop(page, specs, selector=PANEL):
    page.evaluate("() => document.querySelector('.dash-toast')?.remove()")
    dispatch(page, "dragenter", selector, specs)
    dispatch(page, "dragover", selector, specs)
    dispatch(page, "drop", selector, specs)


def settle(page, rows, label):
    """等这一批的每一行都落到终态(不再是等待中 / 上传中)。"""
    try:
        page.wait_for_function(
            "n => { const list = document.querySelector('#dashKbRoot .dash-upload-list');"
            " if (!list) return false;"
            " const chips = Array.from(list.querySelectorAll('.dash-chip'));"
            " return chips.length === n && chips.every(c => !/等待中|上传中/.test(c.textContent)); }",
            arg=rows, timeout=30000)
    except Exception as error:
        check(f"{label}:等到结果", False, str(error).splitlines()[0])
        return page.evaluate(ROWS)
    return page.evaluate(ROWS)


def overview(page):
    return page.evaluate("() => fetch('/api/dash/kb/overview').then(r => r.json())")


def toast(page):
    try:
        page.wait_for_selector(".dash-toast", timeout=4000)
        return page.text_content(".dash-toast")
    except Exception:
        return ""


TEXT = "# 走查\n\n知识库拖放上传的样本文件。\n"

with sync_playwright() as p:
    browser = p.chromium.launch(headless=True)
    page = browser.new_page(viewport={"width": 1440, "height": 900})
    page.on("console", note_console)
    page.on("pageerror", lambda e: problems.append(f"pageerror: {e}"))
    page.on("response", lambda r: http_errors.append((r.status, r.url)) if r.status >= 400 else None)

    page.goto(BASE, wait_until="networkidle")
    page.click("#sidebarSettingsButton")
    page.wait_for_selector('[data-console-panel="settings"]:not([hidden])')
    page.click('.con-rail-item[data-console-panel="kb"]')
    page.wait_for_selector("#dashKbRoot .dash-split")
    shot(page, "01-panel")

    limits = overview(page)
    print(f"· 上限:单文件 {limits['max_file_size_kb']} KB,库里 {limits['file_count']} 个文件")
    check("投放宿主挂在知识库面板上", page.evaluate(AFFORDANCE)["host"])

    # ── 1. 拖文件:指示层出现 ────────────────────────────────
    sample = [{"name": "hover.md", "text": TEXT}]
    dispatch(page, "dragenter", PANEL, sample)
    dispatch(page, "dragover", PANEL, sample)
    state = page.evaluate(AFFORDANCE)
    check("拖文件进来:虚线框 + 遮罩出现", state["dropping"] and state["veilVisible"], json.dumps(state, ensure_ascii=False))
    check("遮罩不吃指针事件", state["pointerEvents"] == "none", state["pointerEvents"])
    shot(page, "02-dragover")

    # ── 2. 嵌套元素不该让指示层闪 ──────────────────────────
    dispatch(page, "dragenter", "#dashKbRoot .dash-tree-pane", sample)
    dispatch(page, "dragleave", PANEL, sample)   # 进子元素时浏览器会给父元素发 dragleave
    mid = page.evaluate(AFFORDANCE)
    check("进子元素后指示层不掉", mid["dropping"] and mid["veilVisible"], json.dumps(mid, ensure_ascii=False))
    dispatch(page, "dragleave", "#dashKbRoot .dash-tree-pane", sample)
    gone = page.evaluate(AFFORDANCE)
    check("最后一层离开后指示层收起", not gone["dropping"] and not gone["veilVisible"], json.dumps(gone, ensure_ascii=False))
    shot(page, "03-after-dragleave")

    # ── 3. 拖文字/链接:既不显示也不吞事件 ──────────────────
    result = dispatch(page, "dragenter", PANEL, None, "https://example.com")
    text_state = page.evaluate(AFFORDANCE)
    check("拖文字不显示指示层", not text_state["dropping"] and not text_state["veilVisible"])
    check("拖文字不 preventDefault(事件照常冒泡出去)", not result["defaultPrevented"], json.dumps(result))
    dispatch(page, "drop", PANEL, None, "https://example.com")
    time.sleep(0.6)
    check("拖文字放下:不弹错、不开上传记录",
          page.evaluate("() => !document.querySelector('.dash-toast') && document.querySelector('#dashKbRoot .dash-upload-log').hidden"))

    # ── 4. 单文件投放 ──────────────────────────────────────
    drop(page, [{"name": "single.md", "text": TEXT}])
    rows = settle(page, 1, "单文件")
    check("单文件投放:成功", rows and rows[0]["text"] == "成功", json.dumps(rows, ensure_ascii=False))
    print(f"  toast: {toast(page)}")
    page.wait_for_selector("#dashKbRoot .dash-tree-row.is-file")
    check("文件树里能看到它", overview(page)["file_count"] == 1)
    shot(page, "04-single-drop")

    # ── 5. 三文件投放 ──────────────────────────────────────
    drop(page, [{"name": f"multi-{i}.md", "text": f"{TEXT}第 {i} 份。\n"} for i in (1, 2, 3)])
    rows = settle(page, 3, "三文件")
    check("三文件投放:三行全成功", [r["text"] for r in rows] == ["成功"] * 3, json.dumps(rows, ensure_ascii=False))
    summary = toast(page)
    check("汇总 toast 说了入库几个", "入库 3 个" in summary, summary)
    print(f"  toast: {summary}")
    shot(page, "05-three-drop")

    # ── 6. 混合投放:2 文本 + 1 png ────────────────────────
    png = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] + [0] * 64
    drop(page, [
        {"name": "mix-a.md", "text": TEXT},
        {"name": "shot.png", "bytes": png, "type": "image/png"},
        {"name": "mix-b.txt", "text": "纯文本样本\n"},
    ])
    rows = settle(page, 3, "混合")
    by_name = {r["name"]: r["text"] for r in rows}
    check("png 被跳过且说清是为什么", by_name.get("shot.png") == "跳过:不是文本文件", json.dumps(by_name, ensure_ascii=False))
    check("同批的两个文本照常入库",
          by_name.get("mix-a.md") == "成功" and by_name.get("mix-b.txt") == "成功", json.dumps(by_name, ensure_ascii=False))
    print(f"  toast: {toast(page)}")
    shot(page, "06-mixed-drop")

    # ── 7. 闸门:超限 + 非 UTF-8 ───────────────────────────
    over_kb = limits["max_file_size_kb"] + 64
    drop(page, [
        {"name": "huge.md", "text": "x" * 1024, "repeat": over_kb},
        {"name": "latin.txt", "bytes": [0xFF, 0xFE, 0x41, 0x00, 0xC3, 0x28]},
    ])
    rows = settle(page, 2, "闸门")
    by_name = {r["name"]: r["text"] for r in rows}
    check("超限文件按配置上限跳过",
          by_name.get("huge.md") == f"跳过:超过 {limits['max_file_size_kb']} KB", json.dumps(by_name, ensure_ascii=False))
    check("非 UTF-8 文件跳过", by_name.get("latin.txt") == "跳过:不是 UTF-8 文本", json.dumps(by_name, ensure_ascii=False))
    shot(page, "07-guardrails")

    # ── 8. 服务端守卫:落点像 Miyu 自己的资产 ──────────────
    drop(page, [{"name": "skill.md", "text": TEXT}])
    rows = settle(page, 1, "被拒")
    rejected = rows[0]["text"] if rows else ""
    check("守卫拒绝时带上服务端原话",
          rejected.startswith("被拒:") and "Miyu keeps its own skills" in rejected, rejected)
    check("被拒的行用 danger 样式", "is-danger" in (rows[0]["cls"] if rows else ""), json.dumps(rows, ensure_ascii=False))
    shot(page, "08-rejected")

    # ── 9. 目录投放:递归 + 层数闸 ─────────────────────────
    page.evaluate(FAKE_TREE)
    drop(page, [{"name": "notes", "text": "dir"}])
    rows = settle(page, 4, "目录")          # 3 个文件 + 1 条层数提示
    page.evaluate(RESTORE_TREE)
    names = [r["name"] for r in rows]
    check("目录被递归展开成带路径的文件名",
          {"notes/a.md", "notes/deep1/b.md", "notes/deep1/deep2/c.md"}.issubset(set(names)),
          json.dumps(names, ensure_ascii=False))
    check("超过 3 层的目录有一条明说的提示",
          any("目录只展开 3 层" in r["text"] for r in rows), json.dumps(rows, ensure_ascii=False))
    check("三个文件都进了库", [r["text"] for r in rows if r["name"].startswith("notes/")] == ["成功"] * 3,
          json.dumps(rows, ensure_ascii=False))
    print(f"  toast: {toast(page)}")
    shot(page, "09-directory-drop")

    # ── 10. 选择器多选:同一条流水线 ───────────────────────
    picked = []
    for index in (1, 2):
        path = OUT / f"picked-{index}.md"
        path.write_text(f"{TEXT}选择器第 {index} 份。\n", encoding="utf-8")
        picked.append(str(path))
    # 输入框是 hidden 的,走 file chooser 才等于用户真点了「上传文件」那颗按钮。
    page.evaluate("() => document.querySelector('.dash-toast')?.remove()")
    with page.expect_file_chooser() as chooser:
        page.click("#dashKbRoot .dash-button.is-primary")
    chooser.value.set_files(picked)
    rows = settle(page, 2, "选择器")
    check("选择器一次两份走同一条流水线", [r["text"] for r in rows] == ["成功"] * 2, json.dumps(rows, ensure_ascii=False))
    check("文件选择器带 multiple",
          page.evaluate("() => document.querySelector('#dashKbRoot input[type=file]:not([webkitdirectory])').multiple"))
    print(f"  toast: {toast(page)}")
    shot(page, "10-picker-multi")

    # ── 收尾 ───────────────────────────────────────────────
    final = overview(page)
    names = sorted(f["name"] for f in final["files"])
    print(f"· 库里 {final['file_count']} 个文件:{json.dumps(names, ensure_ascii=False)}")
    check("入库总数对得上(单 1 + 三 3 + 混 2 + 目录 3 + 选择器 2 = 11)", final["file_count"] == 11, str(final["file_count"]))
    check("skill.md 没进库", "skill.md" not in names)
    check("shot.png 没进库", "shot.png" not in names)
    shot(page, "11-final-tree")

    # ── 11. 覆盖:同名再传一次 ─────────────────────────────
    drop(page, [{"name": "single.md", "text": TEXT + "第二版\n"}])
    rows = settle(page, 1, "覆盖")
    check("同名重传报「已存在:已覆盖」", rows and rows[0]["text"] == "已存在:已覆盖", json.dumps(rows, ensure_ascii=False))
    check("覆盖不增加文件数", overview(page)["file_count"] == 11)
    shot(page, "12-overwrite")

    # ── 12. 窄视口:服务端那句长理由不许把记录行撑破 ───────
    page.set_viewport_size({"width": 430, "height": 900})
    drop(page, [{"name": "config.toml", "text": "a = 1\n"}])
    rows = settle(page, 1, "窄视口被拒")
    check("窄视口下也被拒", rows and rows[0]["text"].startswith("被拒:"), json.dumps(rows, ensure_ascii=False))
    narrow = page.evaluate(
        "() => Array.from(document.querySelectorAll('#dashKbRoot .dash-upload-list li')).map((li) => ({"
        " fits: li.scrollWidth <= li.clientWidth + 1,"
        " nameWidth: Math.round(li.firstElementChild.getBoundingClientRect().width) }))")
    check("长理由被截断而不是撑破行", all(row["fits"] for row in narrow), json.dumps(narrow))
    check("文件名没被理由挤没", all(row["nameWidth"] > 40 for row in narrow), json.dumps(narrow))
    shot(page, "13-narrow-reject")

    browser.close()

reindex_400 = [u for s, u in http_errors if s == 400 and "/api/dash/kb/reindex" in u]
upload_400 = [u for s, u in http_errors if s == 400 and "/api/dash/kb/files" in u]
unexpected = [(s, u) for s, u in http_errors
              if not (s == 400 and any(part in u for part in EXPECTED_400))
              and not (s == 404 and any(part in u for part in EXPECTED_404))]
for status, url in unexpected:
    problems.append(f"http {status}: {url}")
# 上传口只该 400 两次:守卫拒 skill.md 与 config.toml,别的 400 都是回归。
check("上传接口只被拒了两次", len(upload_400) == 2, f"{len(upload_400)} 次")
print(f"· 重建接口 400 {len(reindex_400)} 次(嵌入未配置,前端有意吞掉)")
if problems:
    print("\n没过的项:")
    for item in problems:
        print(f"  - {item}")
    sys.exit(1)
print("\n全部通过")
