// 控制台是 fixed 容器,fullPage 截不到 con-main 内部;这里逐屏滚动截图,并量设置页导航高度。
const { chromium } = require("playwright");
const path = require("path"); const fs = require("fs");
const BASE = process.argv[2] || "http://127.0.0.1:8500"; const W = Number(process.argv[3] || 390), H = Number(process.argv[4] || 844);
const OUT = path.join(__dirname, `scroll-${W}`); fs.mkdirSync(OUT, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await (await browser.newContext({ viewport: { width: W, height: H }, deviceScaleFactor: 2, isMobile: true, hasTouch: true })).newPage();
  page.on("dialog", (d) => d.accept());
  await page.goto(BASE, { waitUntil: "networkidle" }); await sleep(600);
  await page.click("#mobileMenuButton"); await sleep(300); await page.click("#consoleButton");
  await page.waitForSelector("#consoleView:not([hidden])");
  await page.evaluate(() => document.getElementById("consoleView").classList.add("rail-collapsed"));
  const scrollShots = async (name, max = 5) => {
    for (let i = 0; i < max; i++) {
      await sleep(350);
      await page.screenshot({ path: path.join(OUT, `${name}-${i}.png`) });
      const done = await page.evaluate((h) => { const m = document.querySelector(".con-main"); const before = m.scrollTop; m.scrollTop += h - 60; return m.scrollTop === before; }, H);
      if (done) break;
    }
    await page.evaluate(() => { document.querySelector(".con-main").scrollTop = 0; });
  };
  const panel = async (p) => { await page.click(`.con-rail-item[data-console-panel="${p}"]`, { force: true }); await sleep(1200); };
  await panel("usage"); await scrollShots("usage", 4);
  await panel("kb"); await scrollShots("kb", 3);
  await panel("memes"); await scrollShots("memes", 4);
  await panel("settings");
  try { await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 15000 }); } catch (_) {}
  const view = async (v) => { await page.click(`[data-settings-view="${v}"]`, { force: true }); await sleep(500); };
  const navH = {};
  for (const v of ["interface", "prompts", "providers", "mcp", "plugins", "advanced"]) {
    await view(v);
    navH[v] = await page.evaluate(() => { const n = document.querySelector(".settings-nav"); const l = document.querySelector(".settings-layout"); const c = document.querySelector(".settings-content"); return { nav: Math.round(n.getBoundingClientRect().height), layout: Math.round(l.getBoundingClientRect().height), content: Math.round(c.getBoundingClientRect().height), layoutMinH: getComputedStyle(l).minHeight, layoutH: getComputedStyle(l).height, alignContent: getComputedStyle(l).alignContent, rows: getComputedStyle(l).gridTemplateRows }; });
  }
  console.log("settings-nav heights:", JSON.stringify(navH, null, 1));
  await view("general"); await scrollShots("settings-general", 3);
  await view("plugins"); await scrollShots("settings-plugins", 3);
  await view("qq"); await scrollShots("settings-qq", 3);
  await view("mcp"); await scrollShots("settings-mcp", 2);
  await view("advanced"); await scrollShots("settings-advanced", 2);
  await browser.close();
})().catch((e) => { console.error(e); process.exit(1); });
