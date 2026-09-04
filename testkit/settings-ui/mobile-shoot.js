// 手机视口走查:逐面板截图 + 横向溢出探测 + 控制台错误收集。
// 用法:node mobile-shoot.js <baseUrl> <width> <height> <outDir>
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:8500";
const W = Number(process.argv[3] || 390);
const H = Number(process.argv[4] || 844);
const OUT = process.argv[5] || path.join(__dirname, `shots-${W}x${H}`);
fs.mkdirSync(OUT, { recursive: true });

const errors = [];
const overflowReport = [];
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function probeOverflow(page, name) {
  const info = await page.evaluate(() => {
    const vw = window.innerWidth;
    const bad = [];
    const seen = new Set();
    for (const node of document.querySelectorAll("body *")) {
      if (!(node instanceof HTMLElement)) continue;
      const cs = getComputedStyle(node);
      if (cs.display === "none" || cs.visibility === "hidden") continue;
      const r = node.getBoundingClientRect();
      if (r.width === 0 || r.height === 0) continue;
      // 只看视口内可见的、超出右边缘或左边缘的元素
      if (r.bottom < 0 || r.top > window.innerHeight) continue;
      if (r.right > vw + 1 || r.left < -1) {
        // 跳过被祖先 overflow 裁掉的元素(在滚动容器里横向滚是合理的)
        let p = node.parentElement, clipped = false;
        while (p && p !== document.body) {
          const pcs = getComputedStyle(p);
          if (/(auto|scroll|hidden)/.test(pcs.overflowX) && p.getBoundingClientRect().right <= vw + 1) { clipped = true; break; }
          p = p.parentElement;
        }
        if (clipped) continue;
        const key = node.tagName + "." + node.className;
        if (seen.has(key)) continue;
        seen.add(key);
        bad.push({ sel: node.tagName.toLowerCase() + (node.id ? "#" + node.id : "") + (node.className && typeof node.className === "string" ? "." + node.className.trim().split(/\s+/).slice(0, 3).join(".") : ""), left: Math.round(r.left), right: Math.round(r.right), w: Math.round(r.width) });
      }
    }
    return { vw, docW: document.documentElement.scrollWidth, bodyW: document.body.scrollWidth, bad: bad.slice(0, 12) };
  });
  if (info.docW > info.vw + 1 || info.bodyW > info.vw + 1 || info.bad.length) {
    overflowReport.push({ name, ...info });
  }
}

async function shot(page, name) {
  await sleep(500);
  await page.screenshot({ path: path.join(OUT, `${name}.png`) });
  await probeOverflow(page, name);
  console.log("shot", name);
}

async function fullShot(page, name) {
  await sleep(300);
  await page.screenshot({ path: path.join(OUT, `${name}-full.png`), fullPage: true });
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({
    viewport: { width: W, height: H },
    deviceScaleFactor: 2,
    isMobile: true,
    hasTouch: true,
    userAgent: "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
  });
  const page = await ctx.newPage();
  page.on("console", (m) => { if (m.type() === "error") errors.push(`console: ${m.text()}`); });
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("response", (r) => { if (r.status() >= 400) errors.push(`http ${r.status()}: ${r.url()}`); });
  page.on("dialog", (d) => d.accept());
  await page.goto(BASE, { waitUntil: "networkidle" });
  await sleep(800);

  await shot(page, "00-chat-empty");
  // 侧栏抽屉
  await page.click("#mobileMenuButton");
  await shot(page, "01-sidebar-drawer");

  // 分享文件面板
  await page.click("#sharedFilesButton");
  await page.waitForSelector(".shared-files-overlay:not([hidden])");
  await sleep(600);
  await shot(page, "02-shared-files");
  // 勾选一行让批量按钮亮起
  const checks = await page.$$(".shared-files-check");
  if (checks.length) { await checks[0].click(); await shot(page, "03-shared-files-selected"); }
  // 预览一张图
  const preview = await page.$(".shared-files-action:has-text('预览')");
  if (preview) { await preview.click(); await sleep(400); await shot(page, "04-shared-files-preview"); }
  // 记录工具条按钮的实际尺寸
  const toolMetrics = await page.$$eval(".shared-files-tool, .shared-files-action", (nodes) => nodes.map((n) => { const r = n.getBoundingClientRect(); const cs = getComputedStyle(n); return { text: n.textContent.trim(), w: Math.round(r.width), h: Math.round(r.height), radius: cs.borderRadius, lines: Math.round(r.height / parseFloat(cs.lineHeight || 16)) }; }));
  console.log("shared tool metrics:", JSON.stringify(toolMetrics));
  await page.click(".shared-files-close");
  await sleep(300);

  // 控制台
  if (!(await page.$(".sidebar.open"))) { await page.click("#mobileMenuButton"); await sleep(300); }
  await shot(page, "05-after-shared-close");
  await page.click("#consoleButton");
  await page.waitForSelector("#consoleView:not([hidden])");
  if (process.env.COLLAPSE) await page.evaluate(() => document.getElementById("consoleView").classList.add("rail-collapsed"));
  await sleep(800);
  await shot(page, "10-console-usage");
  await fullShot(page, "10-console-usage");

  const panels = ["memory", "kb", "memes", "qq", "groups", "affection"];
  let n = 11;
  for (const p of panels) {
    // rail 在手机上可能藏起来,先看看能不能直接点
    const item = await page.$(`.con-rail-item[data-console-panel="${p}"]`);
    const vis = item && await item.isVisible();
    if (!vis) {
      const toggle = await page.$("#conRailToggle");
      if (toggle && await toggle.isVisible()) { await toggle.click(); await sleep(300); }
    }
    await page.click(`.con-rail-item[data-console-panel="${p}"]`, { force: true });
    await sleep(1200);
    await shot(page, `${n}-console-${p}`);
    await fullShot(page, `${n}-console-${p}`);
    // 子标签
    const segs = await page.$$(`.con-panel[data-console-panel="${p}"] .con-segmented button`);
    let k = 0;
    for (const b of segs.slice(1, 5)) {
      if (!(await b.isVisible())) continue;
      await b.click({ force: true }); await sleep(900); k += 1;
      await shot(page, `${n}-console-${p}-seg${k}`);
      await fullShot(page, `${n}-console-${p}-seg${k}`);
    }
    n += 1;
  }

  // 设置页
  await page.click(`.con-rail-item[data-console-panel="settings"]`, { force: true });
  await page.waitForSelector('[data-console-panel="settings"]:not([hidden])');
  try { await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 15000 }); } catch (_) {}
  await sleep(500);
  const view = async (name) => { await page.click(`[data-settings-view="${name}"]`, { force: true }); await sleep(400); };
  await shot(page, "20-settings-interface");
  await fullShot(page, "20-settings-interface");
  const views = ["prompts", "providers", "models", "general", "mcp", "plugins", "qq", "advanced"];
  let m = 21;
  for (const v of views) {
    await view(v);
    await shot(page, `${m}-settings-${v}`);
    await fullShot(page, `${m}-settings-${v}`);
    m += 1;
  }
  // 供应商抽屉
  await view("providers");
  const card = await page.$(".st-provider-card >> nth=1");
  if (card) {
    await card.click();
    await page.waitForSelector(".st-drawer");
    await sleep(500);
    await shot(page, "30-provider-drawer-models");
    for (const t of ["connection", "advanced"]) {
      const tab = await page.$(`.st-tab[data-tab="${t}"]`);
      if (tab) { await tab.click({ force: true }); await shot(page, `31-provider-drawer-${t}`); }
    }
    const tabModels = await page.$('.st-tab[data-tab="models"]');
    if (tabModels) { await tabModels.click({ force: true }); await sleep(200); }
    const more = await page.$$(".st-model-row .st-icon-btn[title='更多']");
    if (more.length) {
      await more[0].click();
      await page.waitForSelector(".st-menu");
      await shot(page, "32-model-menu");
      await page.click(".st-menu-item >> nth=0");
      try { await page.waitForSelector(".st-popover", { timeout: 3000 }); await shot(page, "33-model-tuner"); } catch (_) {}
      await page.keyboard.press("Escape"); await sleep(250);
    }
    await page.keyboard.press("Escape"); await sleep(400);
  }
  // 人格抽屉
  await view("prompts");
  const persona = await page.$(".st-persona-card >> nth=0");
  if (persona) {
    await persona.click();
    try { await page.waitForSelector(".st-drawer", { timeout: 3000 }); await sleep(400); await shot(page, "34-persona-drawer"); await page.keyboard.press("Escape"); await sleep(400); } catch (_) {}
  }
  // 模型池卡
  await view("models");
  const pool = await page.$(".st-pool-card >> nth=0");
  if (pool) { await shot(page, "35-models-pool"); }

  // 返回聊天,看输入框
  await page.click("#consoleBack", { force: true });
  await sleep(500);
  await shot(page, "40-chat-back");

  fs.writeFileSync(path.join(OUT, "overflow.json"), JSON.stringify(overflowReport, null, 2));
  fs.writeFileSync(path.join(OUT, "errors.txt"), errors.join("\n"));
  console.log("overflow panels:", overflowReport.map((o) => o.name).join(", ") || "(none)");
  console.log("errors:", errors.length);
  await browser.close();
})().catch((e) => { console.error(e); process.exit(1); });
