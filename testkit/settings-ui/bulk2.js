const { chromium } = require("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  page.setDefaultTimeout(15000);
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (m) => { if (m.type() === "error" && !/theme.css|404/.test(m.text())) errors.push(m.text()); });
  await page.goto("http://127.0.0.1:18391/", { waitUntil: "networkidle" });
  await page.click("#sidebarSettingsButton"); await sleep(400);
  const confirmDialog = async () => { await page.waitForSelector(".dash-confirm[open]"); await page.click(".dash-confirm[open] .dash-button.is-danger"); };

  await page.click('.con-rail-item[data-console-panel="kb"]'); await sleep(1500);
  boxes = await page.$$(".dash-tree-check");
  console.log("kb checkboxes:", boxes.length);
  for (let i = 0; i < 2; i += 1) { await page.click(`.dash-tree-check[aria-label="选择 bulk-test-${i + 1}.md"]`); await sleep(150); }
  console.log("kb bulk bar:", await page.textContent(".dash-bulk-bar"));
  await page.screenshot({ path: "shots/bulk-kb.png" });
  await page.click(".dash-bulk-bar .dash-button.is-danger"); await confirmDialog(); await sleep(1500);
  const remaining = [];
  for (const b of await page.$$(".dash-tree-check")) { const label = await b.getAttribute("aria-label"); if (/bulk-test/.test(label)) remaining.push(label); }
  console.log("kb bulk-test remaining:", remaining.length);

  await page.click('.con-rail-item[data-console-panel="memes"]'); await sleep(1500);
  await page.click(".dash-meme:has-text('测试偷来的猫')"); await sleep(500);
  console.log("reason box:", await page.textContent(".dash-meme-reason").catch(() => "none"));
  await page.screenshot({ path: "shots/meme-reason.png" });
  await page.keyboard.press("Escape"); await sleep(300);
  await page.click("button:has-text('选择')"); await sleep(300);
  const cards = await page.$$(".dash-meme.is-selectable");
  await cards[1].click(); await cards[2].click(); await sleep(200);
  await page.click(".dash-bulk-bar button:has-text('禁用')"); await sleep(2500);
  console.log("memes stat cards:", (await page.textContent(".dash-cards")).replace(/\s+/g, " ").slice(0, 160));

  await page.evaluate(() => { document.body.dataset.theme = "linen"; });
  await sleep(400);
  await page.screenshot({ path: "shots/rail-linen.png", clip: { x: 0, y: 0, width: 420, height: 520 } });
  await browser.close();
  console.log("errors:", errors);
})();
