#!/usr/bin/env python3
"""代码块语法高亮的浏览器侧断言 + 三套配色的截图。由 shoot.py 在同一个页面里调用。

分两块看:

  · 结构 —— 助手正文和自己发的消息里,认识的语言要真出多种颜色的 token;不认识
    的语言必须一个 token 都不出、而且正文一字不改(高亮只许切分,不许改字);
    防抖收尾之后不许还剩着没上色的块。
  · 配色 —— 三套色板(晨光 / 夜阑 / 本机真实的 matugen)下逐个角色量对比度。
    语法色是从 MD3 token 派生的,不是抄某套 vendor 主题:抄一套暗色的话,晨光
    下就是白纸上一片看不见的浅字。这一节守的就是这条。

截图落在 MIYU_SYNTAX_SHOTS(默认 ~/.cache/miyu-syntax)。
"""

import os
from pathlib import Path

SHOTS = Path(os.environ.get("MIYU_SYNTAX_SHOTS", Path.home() / ".cache" / "miyu-syntax"))

# 九个上色角色 + diff 的增删行。和 web/styles.css 里的 --tok-* 一一对应。
ROLES = ["c", "p", "o", "k", "s", "n", "f", "t", "y", "ins", "del"]

# 对比度门槛(WCAG 相对亮度比)。数字是先量出来再按实测最差值往下留一档定的,
# 不是拍脑袋:注释和标点本来就该压暗,跟正文用同一档会逼着把它们提亮成噪音。
MIN_CONTRAST_COLORED = 3.6
MIN_CONTRAST_MUTED = 2.4

BLOCK_PROBE = """
() => Array.from(document.querySelectorAll(selectorPlaceholder)).map((block) => {
  const code = block.querySelector("pre code");
  if (!code) return null;
  const spans = Array.from(code.querySelectorAll("span[class^='tok-']"));
  const colors = new Set();
  for (const span of spans) colors.add(getComputedStyle(span).color);
  return {
    language: (code.className.match(/language-([\\w.+-]+)/) || ["", ""])[1],
    roles: [...new Set(spans.map((span) => span.className))].sort(),
    colors: [...colors].sort(),
    spans: spans.length,
    text: code.textContent,
    // 还挂着待办标记 = 防抖那一轮没把它接住。
    pending: code.dataset.hlPending ?? null,
  };
}).filter(Boolean)
"""

# 换主题:body 的 data-theme 决定内置两套,matugen 那套靠 /theme.css 那个 <link>。
THEME_JS = """
(spec) => {
  document.body.dataset.theme = spec.theme;
  const link = document.getElementById("matugenThemeLink");
  if (link) link.disabled = !spec.matugen;
  return { requested: spec, loaded: Boolean(link && link.sheet) };
}
"""

# 逐角色量颜色。探针塞进真实的代码块里,量到的就是它在页面上真正的样子
# (--tok-* 里引用了 --code-bg / --code-ink,脱离上下文量出来的不算数)。
COLOR_JS = """
(roles) => {
  const read = (block) => {
    if (!block) return null;
    const host = block.querySelector("pre code") || block;
    const out = {
      bg: getComputedStyle(block).backgroundColor,
      ink: getComputedStyle(host).color,
    };
    for (const role of roles) {
      const probe = document.createElement("span");
      probe.className = "tok-" + role;
      probe.textContent = "x";
      host.appendChild(probe);
      out[role] = getComputedStyle(probe).color;
      probe.remove();
    }
    return out;
  };
  return {
    assistant: read(document.querySelector(".assistant-content .code-block")),
    user: read(document.querySelector(".user-bubble .code-block")),
  };
}
"""

# 语言样品墙:直接调 MiyuHighlight,一次看全所有语言在当前色板下的样子。
# 用 createElement 搭,不用 innerHTML——被测的东西本身就是「不产生 HTML 字符串」。
GALLERY_JS = """
(samples) => {
  document.getElementById("hlGallery")?.remove();
  const host = document.createElement("div");
  host.id = "hlGallery";
  host.className = "markdown-body";
  host.style.cssText =
    "position:fixed;inset:0;z-index:9998;overflow:hidden;padding:14px;display:grid;" +
    "grid-template-columns:repeat(3,1fr);gap:12px;align-content:start;" +
    "background:var(--app-bg)";
  for (const [language, code] of samples) {
    const wrapper = document.createElement("div");
    wrapper.className = "code-block";
    wrapper.style.margin = "0";
    const toolbar = document.createElement("div");
    toolbar.className = "code-toolbar";
    const label = document.createElement("span");
    label.textContent = language || "代码";
    toolbar.appendChild(label);
    const pre = document.createElement("pre");
    const element = document.createElement("code");
    element.className = "language-" + language;
    element.textContent = code;
    window.MiyuHighlight.paint(element, language, code, true);
    pre.appendChild(element);
    wrapper.append(toolbar, pre);
    host.appendChild(wrapper);
  }
  document.body.appendChild(host);
  const painted = Array.from(host.querySelectorAll("pre code")).map((code) => ({
    language: code.className.replace("language-", ""),
    spans: code.querySelectorAll("span[class^='tok-']").length,
    text: code.textContent,
  }));
  return painted;
}
"""

SAMPLES = [
    ("rust", 'use std::fmt;\n\n/// 文档注释\n#[derive(Debug)]\npub struct Turn { pub id: u64 }\n\nimpl fmt::Display for Turn {\n    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {\n        write!(f, "turn {} ok={}", self.id, true)\n    }\n}'),
    ("python", 'import os\nfrom typing import Optional\n\n\nclass Store:\n    """文档字符串"""\n\n    def get(self, key: str, fallback: Optional[int] = 3) -> str:\n        # 注释\n        return f"{key}={os.environ.get(key, fallback)}"'),
    ("javascript", 'const load = async (id) => {\n  // 注释\n  const res = await fetch(`/api/turns/${id}`, { credentials: "same-origin" });\n  if (!res.ok) throw new Error("boom");\n  return res.json();\n};\nexport default { load, retries: 3, debug: false };'),
    ("typescript", 'interface Turn<T> {\n  id: number;\n  payload?: T;\n}\n\nenum Kind { User = 1, Model }\n\nexport const pick = <T,>(t: Turn<T>): T | null => t.payload ?? null;'),
    ("bash", '#!/usr/bin/env bash\n# 注释\nset -euo pipefail\nexport MIYU_HOME="/tmp/mx"\nfor file in web/*.js; do\n  node --check "$file" || exit 1\ndone\necho "done $?"'),
    ("json", '{\n  "active_provider": "stub",\n  "providers": [{ "id": "stub", "models": ["stub-model"] }],\n  "memory": { "enabled": false, "top_k": 8 }\n}'),
    ("toml", '# 注释\n[package]\nname = "miyu"\nversion = "0.5.0"\nedition = "2021"\n\n[profile.release]\ncodegen-units = 1\nlto = true'),
    ("yaml", '# 注释\nname: miyu\njobs:\n  build:\n    steps:\n      - run: cargo check\n        env: { RUST_LOG: "info" }\nanchor: &base 1\nuse: *base'),
    ("sql", "-- 注释\nSELECT id, role, created_at\nFROM turns\nWHERE session_id = 'abc' AND created_at > 1700000000\nORDER BY id DESC\nLIMIT 20;"),
    ("c", '#include <stdio.h>\n\n/* 注释 */\nint main(void) {\n    const char *name = "miyu";\n    printf("hello %s %d\\n", name, 42);\n    return 0;\n}'),
    ("cpp", '#include <vector>\n#include <string>\n\ntemplate <class T>\nstruct Bag {\n    std::vector<T> items;\n    void add(const T &v) { items.push_back(v); }\n};'),
    ("go", 'package main\n\nimport "fmt"\n\n// 注释\nfunc main() {\n\tcounts := map[string]int{"miyu": 1}\n\tfor k, v := range counts {\n\t\tfmt.Printf("%s=%d\\n", k, v)\n\t}\n}'),
    ("lua", '-- 注释\nlocal store = { count = 0 }\n\nfunction store:bump(step)\n  self.count = self.count + (step or 1)\n  return "count=" .. self.count\nend'),
    ("markdown", "# 标题\n\n正文里有 **加粗**、_斜体_ 和 `行内代码`。\n\n- 列表项\n- [链接](https://example.com)\n\n> 引用\n"),
    ("diff", "--- a/web/app.js\n+++ b/web/app.js\n@@ -3545,7 +3545,9 @@\n-    code.textContent = codeText;\n+    code.textContent = codeText;\n+    window.MiyuHighlight?.paint(code, language, codeText, settled);\n     pre.appendChild(code);"),
    ("html", '<!doctype html>\n<div class="card" data-id="1">\n  <!-- 注释 -->\n  <span>文字</span>\n  <script>var ready = true;</script>\n</div>'),
    ("css", '/* 注释 */\n.code-block pre {\n  color: var(--code-ink);\n  font-size: 12.5px !important;\n}\n\n@media (max-width: 720px) {\n  .code-block { margin: 0; }\n}'),
    ("zzunknownlang", 'unknown language <b>stays</b> & "plain"'),
]


def srgb(value):
    """计算样式的颜色 → (r, g, b)，0–255。

    三种形态都要认:`rgb(r, g, b)` / `rgba(...)`,以及 **`color(srgb r g b)`**
    ——后者是 `color-mix()` 的计算值形态,分量是 0–1 的小数而不是 0–255,而且
    括号里第一个词是 `srgb`。只按 rgb() 解析会当场 ValueError(09-09 撞过两次:
    一次在链接卡片的对比度探针,一次在这里)。
    """
    inner = value[value.index("(") + 1:value.rindex(")")]
    inner = inner.replace("/", " ").replace(",", " ")
    parts = inner.split()
    if parts and not parts[0].replace(".", "", 1).replace("-", "", 1).isdigit():
        # color(srgb 0.86 0.89 0.97 / 0.14) —— 去掉色彩空间名,分量是 0–1。
        parts = parts[1:]
        return tuple(float(p) * 255.0 for p in parts[:3])
    return tuple(float(p) for p in parts[:3])


def luminance(color):
    def channel(v):
        v /= 255.0
        return v / 12.92 if v <= 0.04045 else ((v + 0.055) / 1.055) ** 2.4
    r, g, b = (channel(c) for c in color)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(fg, bg):
    a, b = luminance(srgb(fg)), luminance(srgb(bg))
    lo, hi = min(a, b), max(a, b)
    return (hi + 0.05) / (lo + 0.05)


def distinct(colors):
    """按感知距离数「真的分得开」的颜色数。同一角色重复出现不算。"""
    kept = []
    for color in colors:
        rgb = srgb(color)
        if all(sum(abs(a - b) for a, b in zip(rgb, other)) > 24 for other in kept):
            kept.append(rgb)
    return len(kept)


def run(page, check, user_code):
    SHOTS.mkdir(parents=True, exist_ok=True)

    # ── 结构 ────────────────────────────────────────────────
    def blocks(selector):
        return page.evaluate(BLOCK_PROBE.replace("selectorPlaceholder", repr(selector)))

    assistant = blocks(".assistant-content .code-block")
    mine = blocks(".user-bubble .code-block")

    check("助手正文里有代码块", len(assistant) >= 4, f"{len(assistant)} 块")
    rust = next((b for b in assistant if b["language"] == "rust"), None)
    if rust is None:
        check("助手的 rust 代码块上了色", False, str([b["language"] for b in assistant]))
    else:
        check("助手的 rust 代码块上了色", distinct(rust["colors"]) > 1,
              f"{distinct(rust['colors'])} 种颜色 / {len(rust['roles'])} 类 token:{' '.join(rust['roles'])}")
        check("上色没改动代码正文", "HashMap<&str, u32>" in rust["text"] and rust["text"].endswith("}"),
              repr(rust["text"][-40:]))

    diff = next((b for b in assistant if b["language"] == "diff"), None)
    check("diff 的增删行分得出来",
          diff is not None and "tok-ins" in diff["roles"] and "tok-del" in diff["roles"],
          str(diff["roles"]) if diff else "没有 diff 块")

    unknown = next((b for b in assistant if b["language"] == "zzunknownlang"), None)
    if unknown is None:
        check("不认识的语言退回纯文本", False, str([b["language"] for b in assistant]))
    else:
        check("不认识的语言退回纯文本", unknown["spans"] == 0, f"{unknown['spans']} 个 token")
        check("不认识的语言正文一字未改",
              unknown["text"] == 'this is not a real language <b>x</b> & "quoted"',
              repr(unknown["text"]))

    bare = [b for b in assistant if not b["language"]]
    check("没标语言的围栏不上色", all(b["spans"] == 0 for b in bare), f"{len(bare)} 块")
    check("防抖收尾后没有欠着的代码块",
          all(b["pending"] is None for b in assistant + mine),
          str([b["language"] for b in assistant + mine if b["pending"] is not None]))

    check("自己发的 sh 代码块上了色", bool(mine) and distinct(mine[0]["colors"]) > 1,
          f"{distinct(mine[0]['colors']) if mine else 0} 种颜色:{' '.join(mine[0]['roles']) if mine else ''}")
    check("自己发的代码块上色后一字未改", bool(mine) and mine[0]["text"] == user_code,
          repr(mine[0]["text"]) if mine else "没有代码块")

    # ── 样品墙:所有语言过一遍,顺便验不认识的语言不炸 ─────────
    painted = page.evaluate(GALLERY_JS, SAMPLES)
    by_language = {item["language"]: item for item in painted}
    silent = sorted(name for name, item in by_language.items()
                    if item["spans"] == 0 and name != "zzunknownlang")
    check("每门声明支持的语言都真的出了 token", not silent, " ".join(silent))
    check("样品墙的正文全都一字未改",
          all(item["text"] == code for (language, code), item in zip(SAMPLES, painted)),
          str([language for (language, code), item in zip(SAMPLES, painted)
               if item["text"] != code]))
    page.evaluate("() => document.getElementById('hlGallery')?.remove()")

    # ── 配色:逐套色板量对比度,顺手把两侧代码块拍下来 ────────
    palettes = [("linen", "linen", False), ("graphite", "graphite", False),
                ("matugen-dark", "graphite", True), ("matugen-light", "linen", True)]
    available = []
    for name, theme, matugen in palettes:
        applied = page.evaluate(THEME_JS, {"theme": theme, "matugen": matugen})
        if matugen and not applied["loaded"]:
            print(f"! 沙箱里没有 /theme.css，跳过 {name}")
            continue
        available.append((name, theme, matugen))
        page.wait_for_timeout(220)
        colors = page.evaluate(COLOR_JS, ROLES)
        for side in ("assistant", "user"):
            palette = colors[side]
            if not palette:
                check(f"{name}/{side} 量得到代码块", False)
                continue
            worst, worst_role = 99.0, ""
            for role in ROLES:
                ratio = contrast(palette[role], palette["bg"])
                floor = MIN_CONTRAST_MUTED if role in ("c", "p") else MIN_CONTRAST_COLORED
                if ratio < floor:
                    check(f"{name}/{side} tok-{role} 对比度", False,
                          f"{ratio:.2f}:1 < {floor}（{palette[role]} on {palette['bg']}）")
                if ratio < worst:
                    worst, worst_role = ratio, role
            spread = distinct([palette[role] for role in ("k", "s", "n", "f", "t", "y")])
            check(f"{name}/{side} 六个上色角色分得开", spread >= 4,
                  f"{spread}/6 种，最低对比度 tok-{worst_role} {worst:.2f}:1")
            print(f"    {name}/{side} 对比度：" + " ".join(
                f"{role}={contrast(palette[role], palette['bg']):.1f}" for role in ROLES))
        shoot_block(page, ".assistant-content .code-block", SHOTS / f"{name}-assistant.png")
        shoot_block(page, ".user-message .user-bubble", SHOTS / f"{name}-user.png")

    # ── 语言样品墙:一屏放不下,单独换个大视口拍 ───────────────
    check("高亮模块挂上了", page.evaluate("() => Boolean(window.MiyuHighlight)"))
    page.set_viewport_size({"width": 1560, "height": 2100})
    for name, theme, matugen in available:
        page.evaluate(THEME_JS, {"theme": theme, "matugen": matugen})
        page.evaluate(GALLERY_JS, SAMPLES)
        page.wait_for_timeout(220)
        page.locator("#hlGallery").screenshot(path=str(SHOTS / f"{name}-gallery.png"))
        page.evaluate("() => document.getElementById('hlGallery')?.remove()")
    page.set_viewport_size({"width": 1280, "height": 900})
    print(f"  shot {len(available)} 套色板 × (gallery/assistant/user) → {SHOTS}")

    # 收摊:主题拨回默认,免得后面的走查在别的配色下截图。
    page.evaluate(THEME_JS, {"theme": "graphite", "matugen": False})
    page.wait_for_timeout(150)


def shoot_block(page, selector, path):
    node = page.query_selector(selector)
    if node is None:
        return
    node.scroll_into_view_if_needed()
    page.wait_for_timeout(150)
    node.screenshot(path=str(path))
