// 模型池页 / QQ 配置模型 / 池引用弹出框 的真机截图。
// 用法:NODE_PATH=<testkit>/node_modules node shoot-pools.js [baseUrl] [outDir]
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:18391";
const SHOTS = process.argv[3] || path.join(__dirname, "shots");
fs.mkdirSync(SHOTS, { recursive: true });
const errors = [];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
async function shot(page, name) { await sleep(400); await page.screenshot({ path: path.join(SHOTS, `${name}.png`) }); console.log("shot", name); }

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  page.on("console", (m) => { if (m.type() === "error") errors.push(`console: ${m.text()}`); });
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));
  page.on("dialog", (d) => d.accept());
  await page.goto(BASE, { waitUntil: "networkidle" });
  await page.click("#sidebarSettingsButton");
  await page.waitForSelector('[data-console-panel="settings"]:not([hidden])');
  await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 15000 });
  const view = async (name) => { await page.click(`[data-settings-view="${name}"]`); await sleep(150); };

  await view("models"); await shot(page, "pools-01-page");
  const texts = await page.$$eval("#settings-models .st-pool-head h3, #settings-models .st-pool-section-head h3, #settings-models .st-pool-member-copy", (n) => n.map((x) => x.textContent.trim()));
  console.log("models page texts:", JSON.stringify(texts));
  const selects = await page.$$eval("#settings-models select", (n) => n.map((s) => Array.from(s.options).map((o) => o.textContent)));
  console.log("aux selects:", JSON.stringify(selects[0] || []));
  const tierAdd = await page.$(".st-pool-grid.is-tiers .st-pool-card >> nth=0 >> text=添加模型");
  if (tierAdd) { await tierAdd.click(); await page.waitForSelector(".st-popover"); await shot(page, "pools-02-tier-add"); await page.keyboard.press("Escape"); await sleep(200); }

  await view("qq");
  const card = await page.$(".st-card:has-text('配置模型')");
  if (card) {
    await card.scrollIntoViewIfNeeded(); await shot(page, "pools-03-qq-models");
    const labels = await card.$$eval(".st-picker-text, .st-field-label, label, strong", (n) => n.map((x) => x.textContent.trim()).filter(Boolean).slice(0, 20));
    console.log("qq models card texts:", JSON.stringify(labels));
    const picker = await card.$(".st-picker");
    if (picker) {
      await picker.click(); await page.waitForSelector(".st-popover");
      const rows = await page.$$eval(".st-popover .st-check-row", (n) => n.map((x) => x.textContent.trim()).slice(0, 10));
      console.log("popover rows:", JSON.stringify(rows));
      await shot(page, "pools-04-qq-pool-ref-popover"); await page.keyboard.press("Escape"); await sleep(200);
    }
    const third = (await card.$$(".st-picker"))[2];
    if (third) { await third.click(); await page.waitForSelector(".st-popover"); await shot(page, "pools-05-qq-nonwhitelist-popover"); await page.keyboard.press("Escape"); await sleep(200); }
  } else console.log("no 配置模型 card");
  const rc = await page.$(".st-plugin-open:has-text('群聊真实上下文回复')");
  if (rc) {
    await rc.click(); await page.waitForSelector(".st-drawer"); await shot(page, "pools-06-real-context");
    const picker = await page.$(".st-drawer .st-picker");
    if (picker) { await picker.click(); await page.waitForSelector(".st-popover"); await shot(page, "pools-07-real-context-popover"); await page.keyboard.press("Escape"); await sleep(200); }
    await page.keyboard.press("Escape"); await sleep(300);
  }
  await page.setViewportSize({ width: 900, height: 900 }); await view("models"); await shot(page, "pools-08-page-900");
  await page.setViewportSize({ width: 430, height: 900 }); await view("models"); await shot(page, "pools-09-page-430");
  console.log("errors:", errors.length ? errors : "none");
  await browser.close();
})().catch((e) => { console.error(e); process.exit(1); });
