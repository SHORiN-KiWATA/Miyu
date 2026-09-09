// 控制台深链（#console/<面板>[/<设置子页>]）+ 设置页宽屏居中 的走查。
//
// 验两件事：
//   1. 控制台停在哪个面板，刷新后还在哪个面板（09-09 之前刷新一律回聊天页：
//      initialize 认 hash 但全站没人写过 hash）。
//   2. 设置页在宽屏下整块居中（之前 .settings-layout 贴左铺满，右边留白）。
//
// 用法：node console-hash-shoot.js <baseUrl> [outDir]
// 前置：daemon 起在 baseUrl（沙箱 MIYU_HOME，见 console-hash-run.py），
//       且 `cargo build` 过——web/*.js 与 styles.css 编进二进制。
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:18411";
const OUT = process.argv[3] || path.join(__dirname, "shots-console-hash");
fs.mkdirSync(OUT, { recursive: true });

const errors = [];
const failures = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// 已知的既有噪声，本次改动之前就有（拿 0.5.0 发布版二进制同一脚本对照过）：
//  · theme.css 404：沙箱 home 没有 matugen 主题文件，页面按 404 走退路。
//  · 开控制台时一条 inline style 的 CSP 拦截：统计图那边的老问题，与深链无关。
const KNOWN_NOISE = [/\/theme\.css/, /Failed to load resource.*404/, /Applying inline style violates/];
const isNoise = (text) => KNOWN_NOISE.some((rule) => rule.test(text));

function check(name, ok, detail) {
  console.log(`${ok ? "  ok " : "FAIL "} ${name}${detail ? "  " + detail : ""}`);
  if (!ok) failures.push(`${name} — ${detail || ""}`);
}

async function shot(page, name) {
  await sleep(300);
  await page.screenshot({ path: path.join(OUT, `${name}.png`) });
}

/** 元素存在且没被 hidden 属性/display:none 藏起来。 */
async function visible(page, selector) {
  return page.evaluate((sel) => {
    const node = document.querySelector(sel);
    if (!node) return false;
    return !node.hidden && getComputedStyle(node).display !== "none";
  }, selector);
}

async function consoleVisible(page) {
  return visible(page, "#consoleView");
}

/** 浏览器把「只差 hash」当同文档导航，不会重跑脚本；深链要验的正是启动路径。 */
async function gotoHash(page, hash) {
  await page.goto(`${BASE}/${hash}`, { waitUntil: "networkidle" });
  await page.reload({ waitUntil: "networkidle" });
  await sleep(1000);
}

// 手机断点用：沿用 mobile-shoot.js 的横向溢出探测，范围收到设置面板内
// ——本次只动了 .settings-layout，页面别处的抽屉（侧栏收起时整块推到视口
// 左外）本来就"溢出"，不该算在这一项头上。
async function probeOverflow(page) {
  return page.evaluate(() => {
    const vw = window.innerWidth;
    const bad = [];
    const root = document.querySelector('.con-panel[data-console-panel="settings"]');
    if (!root) return { vw, docW: document.documentElement.scrollWidth, bad: ["settings panel missing"] };
    for (const node of root.querySelectorAll("*")) {
      if (!(node instanceof HTMLElement)) continue;
      const cs = getComputedStyle(node);
      if (cs.display === "none" || cs.visibility === "hidden") continue;
      const r = node.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) continue;
      if (r.bottom < 0 || r.top > window.innerHeight) continue;
      if (r.right > vw + 1 || r.left < -1) {
        let p = node.parentElement, clipped = false;
        while (p && p !== document.body) {
          const pcs = getComputedStyle(p);
          if (/(auto|scroll|hidden)/.test(pcs.overflowX) && p.getBoundingClientRect().right <= vw + 1) { clipped = true; break; }
          p = p.parentElement;
        }
        if (clipped) continue;
        bad.push(node.tagName.toLowerCase() + (node.className && typeof node.className === "string" ? "." + node.className.trim().split(/\s+/)[0] : ""));
      }
    }
    return { vw, docW: document.documentElement.scrollWidth, bad: bad.slice(0, 8) };
  });
}

async function openConsole(page) {
  await page.click("#consoleButton");
  await page.waitForSelector("#consoleView:not([hidden])");
  await sleep(400);
}

/** 深链没生效时（修复前的基线就是这样）手动补位，后面的步骤才跑得下去。 */
async function recoverConsole(page, panel, view) {
  if (!(await consoleVisible(page))) await openConsole(page);
  await page.click(`.con-rail-item[data-console-panel="${panel}"]`);
  await sleep(600);
  if (view) {
    await page.click(`[data-settings-view="${view}"]`);
    await sleep(500);
  }
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  const page = await ctx.newPage();
  page.on("console", (m) => { if (m.type() === "error" && !isNoise(m.text())) errors.push(`console: ${m.text()}`); });
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("response", (r) => { if (r.status() >= 400 && !isNoise(r.url())) errors.push(`http ${r.status()}: ${r.url()}`); });

  // ── 1. 面板深链：切到 QQ 面板 → 刷新 → 还在 QQ 面板 ──
  await page.goto(BASE, { waitUntil: "networkidle" });
  await sleep(600);
  check("初始没有 hash", page.url().indexOf("#") === -1, page.url());
  await openConsole(page);
  check("开控制台写了 #console/usage", (new URL(page.url())).hash === "#console/usage", page.url());

  await page.click('.con-rail-item[data-console-panel="qq"]');
  await sleep(600);
  check("切到 QQ 面板 hash 跟着变", (new URL(page.url())).hash === "#console/qq", page.url());
  await page.reload({ waitUntil: "networkidle" });
  await sleep(900);
  check("刷新后控制台还开着", await consoleVisible(page));
  check("刷新后停在 QQ 面板", await visible(page, '.con-panel[data-console-panel="qq"]'));
  await shot(page, "01-qq-after-reload");
  await recoverConsole(page, "qq");

  // ── 2. 设置子页深链 ──
  await page.click('.con-rail-item[data-console-panel="settings"]');
  await page.waitForSelector('[data-console-panel="settings"]:not([hidden])');
  await sleep(500);
  await page.click('[data-settings-view="qq"]');
  await sleep(400);
  check("设置子页写进 hash", (new URL(page.url())).hash === "#console/settings/qq", page.url());
  await page.reload({ waitUntil: "networkidle" });
  await sleep(1200);
  check("刷新后还在设置页", await visible(page, '.con-panel[data-console-panel="settings"]'));
  check("刷新后还在 QQ 子页", await visible(page, '[data-settings-panel="qq"]'));
  await shot(page, "02-settings-qq-after-reload");
  await recoverConsole(page, "settings", "qq");

  // ── 3. 返回聊天：hash 清空，刷新不再进控制台 ──
  await page.click("#consoleBack");
  await sleep(400);
  check("返回聊天后 hash 清空", (new URL(page.url())).hash === "", page.url());
  await page.reload({ waitUntil: "networkidle" });
  await sleep(800);
  check("刷新后停在聊天页", !(await consoleVisible(page)));
  await shot(page, "03-chat-after-close");

  // ── 4. 老约定：裸 #console 仍开数据统计；未知面板名落回统计 ──
  await gotoHash(page, "#console");
  check("裸 #console 开控制台", await consoleVisible(page));
  check("裸 #console 开数据统计", await visible(page, '.con-panel[data-console-panel="usage"]'));
  await gotoHash(page, "#console/nosuchpanel");
  check("未知面板名开控制台", await consoleVisible(page));
  check("未知面板名落回数据统计", await visible(page, '.con-panel[data-console-panel="usage"]'));

  // ── 5. 宽屏 2000×1100：设置页整块居中 ──
  await page.setViewportSize({ width: 2000, height: 1100 });
  // 用「全局」子页量:界面页是老版式，没有 .st-root 这一层。
  await gotoHash(page, "#console/settings/general");
  check("宽屏深链开在设置页", await visible(page, '.con-panel[data-console-panel="settings"]'));
  await recoverConsole(page, "settings", "general"); // 深链没生效也要量到布局
  await page.waitForSelector('[data-settings-panel="general"] .st-root .st-card', { timeout: 15000 });
  const wide = await page.evaluate(() => {
    const layout = document.querySelector(".settings-layout");
    const panel = document.querySelector('.con-panel[data-console-panel="settings"]');
    // 选择器清单按文档顺序取首个匹配,写成 "A, B" 会先命中隐藏子页里的
    // .st-root(宽 0);只认当前显示的那一页。
    const root = document.querySelector('[data-settings-panel="general"] .st-root');
    const l = layout.getBoundingClientRect(), p = panel.getBoundingClientRect();
    // 外壳整体 zoom: var(--ui-scale)(默认 1.1),getBoundingClientRect 量到的
    // 是缩放后的值;要跟 CSS 里的 1180/960 对账得先除掉。
    const zoom = Number(getComputedStyle(document.documentElement).getPropertyValue("--ui-scale")) || 1;
    return {
      zoom,
      lw: l.width / zoom,
      pw: p.width / zoom,
      rootW: root ? root.getBoundingClientRect().width / zoom : 0,
      left: l.left - p.left,
      right: p.right - l.right,
    };
  });
  check("宽屏下设置区宽 ≈1180", Math.abs(wide.lw - 1180) <= 2, JSON.stringify(wide));
  check("宽屏下左右留白相等", Math.abs(wide.left - wide.right) <= 2, JSON.stringify(wide));
  check("内容列填满 .st-root 的 960 上限", Math.abs(wide.rootW - 960) <= 2, JSON.stringify(wide));
  await shot(page, "04-settings-wide");

  // ── 6. 1100×800：没到上限就铺满 ──
  await page.setViewportSize({ width: 1100, height: 800 });
  await page.reload({ waitUntil: "networkidle" });
  await sleep(1000);
  await recoverConsole(page, "settings", "general");
  const narrow = await page.evaluate(() => {
    const l = document.querySelector(".settings-layout").getBoundingClientRect();
    const p = document.querySelector('.con-panel[data-console-panel="settings"]').getBoundingClientRect();
    return { lw: l.width, pw: p.width };
  });
  check("窄于上限时铺满面板", Math.abs(narrow.lw - narrow.pw) <= 1, JSON.stringify(narrow));
  await shot(page, "05-settings-narrow");

  // ── 7. 手机 390×844：导航横向一排、无横向溢出 ──
  await page.setViewportSize({ width: 390, height: 844 });
  await page.reload({ waitUntil: "networkidle" });
  await sleep(1200);
  await recoverConsole(page, "settings", "general");
  const nav = await page.evaluate(() => getComputedStyle(document.querySelector(".settings-nav")).display);
  check("手机断点导航是 flex 横排", nav === "flex", nav);
  const overflow = await probeOverflow(page);
  check("手机断点无横向溢出", overflow.docW <= overflow.vw + 1 && overflow.bad.length === 0, JSON.stringify(overflow));
  await shot(page, "06-settings-mobile");

  fs.writeFileSync(path.join(OUT, "errors.txt"), errors.join("\n"));
  console.log(`\n控制台错误 ${errors.length} 条${errors.length ? ":\n" + errors.join("\n") : ""}`);
  console.log(`断言失败 ${failures.length} 条${failures.length ? ":\n" + failures.join("\n") : ""}`);
  console.log(`截图在 ${OUT}`);
  await browser.close();
  process.exit(failures.length || errors.length ? 1 : 0);
})().catch((e) => { console.error(e); process.exit(1); });
