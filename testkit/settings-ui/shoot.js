// 设置页重做的真机走查:逐页截图 + 抽屉/菜单/弹出框/对话框各开一遍,收集控制台错误。
// 用法:NODE_PATH=$(npm root -g) node shoot.js [baseUrl]
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:18391";
const SHOTS = path.join(__dirname, "shots");
fs.mkdirSync(SHOTS, { recursive: true });

const errors = [];
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function shot(page, name) {
  await sleep(450);
  await page.screenshot({ path: path.join(SHOTS, `${name}.png`) });
  console.log("shot", name);
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 });
  page.on("console", (message) => { if (message.type() === "error") errors.push(`console: ${message.text()}`); });
  page.on("pageerror", (error) => errors.push(`pageerror: ${error.message}`));
  page.on("response", (response) => { if (response.status() >= 400) errors.push(`http ${response.status()}: ${response.url()}`); });
  page.on("dialog", (dialog) => dialog.accept());
  await page.goto(BASE, { waitUntil: "networkidle" });
  await page.click("#sidebarSettingsButton");
  await page.waitForSelector('[data-console-panel="settings"]:not([hidden])');
  await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 15000 });

  const view = async (name) => { await page.click(`[data-settings-view="${name}"]`); await sleep(120); };

  await shot(page, "01-interface");
  await view("prompts"); await shot(page, "02-prompts");
  await view("providers"); await shot(page, "03-providers");

  // 供应商抽屉:模型页 → 连接页 → 单模型菜单 → 微调弹出框
  await page.click(".st-provider-card >> nth=1");
  await page.waitForSelector(".st-drawer");
  await shot(page, "04-provider-drawer-models");
  await page.click('.st-tab[data-tab="connection"]'); await shot(page, "05-provider-drawer-connection");
  await page.click('.st-tab[data-tab="advanced"]'); await shot(page, "06-provider-drawer-advanced");
  await page.click('.st-tab[data-tab="models"]'); await sleep(200);
  const moreButtons = await page.$$(".st-model-row .st-icon-btn[title='更多']");
  if (moreButtons.length) {
    await moreButtons[0].click();
    await page.waitForSelector(".st-menu");
    await shot(page, "07-model-menu");
    await page.click(".st-menu-item >> nth=0");
    await page.waitForSelector(".st-popover");
    await shot(page, "08-model-tuner");
    await page.keyboard.press("Escape"); await sleep(250);
  }
  await page.keyboard.press("Escape"); await sleep(350);

  // 真实拉取:找一张有密钥的 HTTP 供应商卡(DeepSeek),拉取 /models 并加入所选
  const deepseek = await page.$(".st-provider-card:has-text('DeepSeek'), .st-provider-card:has-text('deepseek')");
  if (deepseek) {
    await deepseek.click();
    await page.waitForSelector(".st-drawer");
    await page.click(".st-drawer >> text=拉取模型列表");
    try {
      await page.waitForSelector(".st-dialog[open] .st-fetch-row", { timeout: 40000 });
      await shot(page, "04b-fetch-dialog");
      const rows = await page.$$(".st-dialog[open] .st-fetch-row");
      console.log("fetched rows:", rows.length);
      await page.click(".st-dialog[open] >> text=全选可见");
      const selectedText = await page.textContent(".st-dialog[open] .st-toolbar-hint");
      console.log("selection:", selectedText);
      if (/已选 0/.test(selectedText || "")) await page.click(".st-dialog[open] >> text=取消");
      else await page.click(".st-dialog[open] >> text=加入所选");
      await page.waitForSelector(".st-dialog[open]", { state: "detached", timeout: 5000 });
      await sleep(300);
      await shot(page, "04c-after-add");
      const modelRows = await page.$$eval(".st-drawer .st-model-row", (nodes) => nodes.map((node) => node.textContent.trim().slice(0, 80)));
      console.log("model rows after add:", modelRows.length, modelRows.slice(0, 4));
    } catch (error) {
      console.log("fetch dialog did not appear:", error.message);
      await shot(page, "04b-fetch-failed");
    }
    await page.keyboard.press("Escape"); await sleep(350);
  } else {
    console.log("no DeepSeek card");
  }

  // 添加供应商:直接建空白供应商并打开抽屉
  await page.click("text=添加供应商");
  await page.waitForSelector(".st-drawer");
  await shot(page, "09-add-provider-drawer");
  await page.keyboard.press("Escape"); await sleep(400);
  // 关抽屉后卡片不该重放入场动画
  const replay = await page.evaluate(() => document.getAnimations().filter((a) => a.animationName === "st-fade-up").length);
  console.log("st-fade-up animations after drawer close:", replay);

  await view("models"); await shot(page, "10-model-pools");
  await page.click(".st-pool-card >> nth=2 >> text=添加模型");
  await page.waitForSelector(".st-popover");
  await shot(page, "10b-pool-add-popover");
  const box = await page.$(".st-popover input[type=checkbox]");
  if (box) { await box.click(); await sleep(250); await shot(page, "10c-pool-added"); await box.click(); }
  await page.keyboard.press("Escape"); await sleep(200);

  await view("general");
  await shot(page, "11-general");
  const disclosure = await page.$(".st-disclosure-head");
  if (disclosure) { await disclosure.click(); await sleep(350); await shot(page, "12-general-advanced-open"); }

  await view("mcp"); await shot(page, "13-mcp");
  await page.click("text=添加服务器");
  await page.waitForSelector(".st-dialog[open]");
  await shot(page, "14-mcp-dialog");
  await page.keyboard.press("Escape"); await sleep(250);

  await view("plugins"); await shot(page, "15-plugins");
  await page.click(".st-plugin-open:has-text('识图')");
  await page.waitForSelector(".st-drawer");
  await shot(page, "16-plugin-vision-drawer");
  const picker = await page.$(".st-drawer .st-picker");
  if (picker) { await picker.click(); await page.waitForSelector(".st-menu, .st-popover"); await shot(page, "17-plugin-model-ref-menu"); await page.keyboard.press("Escape"); await sleep(200); }
  await page.keyboard.press("Escape"); await sleep(350);
  await page.click(".st-plugin-open:has-text('网络搜索')");
  await page.waitForSelector(".st-drawer");
  await shot(page, "18-plugin-web-drawer");
  await page.keyboard.press("Escape"); await sleep(350);

  await view("qq"); await shot(page, "19-qq-top");
  await page.evaluate(() => { document.querySelector(".settings-content").scrollTop = 99999; });
  await shot(page, "20-qq-bottom");
  const whitelist = await page.$("#settings-qq .st-chips");
  if (whitelist) { await whitelist.scrollIntoViewIfNeeded(); await shot(page, "21-qq-chips"); }
  const limit = await page.$("#settings-qq .st-picker.is-compact");
  if (limit) { await limit.scrollIntoViewIfNeeded(); await limit.click(); await page.waitForSelector(".st-popover"); await shot(page, "22-qq-rate-limit-popover"); await page.keyboard.press("Escape"); await sleep(200); }
  await page.click(".st-plugin-open:has-text('群聊真实上下文回复')");
  await page.waitForSelector(".st-drawer");
  await shot(page, "23-real-context-basic");
  await page.click('.st-tab[data-tab="affection"]'); await shot(page, "24-real-context-affection");
  await page.click('.st-tab[data-tab="identity"]'); await shot(page, "25-real-context-identity");
  await page.keyboard.press("Escape"); await sleep(350);
  await page.click(".st-plugin-open:has-text('定时消息')");
  await page.waitForSelector(".st-drawer");
  await page.click("text=新增任务");
  await page.waitForSelector(".st-dialog[open]");
  await shot(page, "26-scheduled-task-dialog");
  await page.keyboard.press("Escape"); await sleep(250);
  await page.keyboard.press("Escape"); await sleep(350);
  const route = await page.$("#settings-qq .st-route-row");
  if (route) { await route.click(); await page.waitForSelector(".st-drawer"); await shot(page, "27-route-drawer"); await page.keyboard.press("Escape"); await sleep(350); }

  await view("advanced"); await shot(page, "28-advanced");

  // 改一项 → 底栏升起 → 保存 → 校验回读
  await view("general");
  const maxRounds = await page.$('#settings-general input[aria-label="最大工具轮数"], #settings-general input[aria-label="工具最大轮数"]');
  if (maxRounds) {
    const before = await maxRounds.inputValue();
    await maxRounds.fill(String(Number(before || 0) + 1));
    await sleep(200);
    await shot(page, "29-dirty-footer");
    const status = await page.textContent("#settingsStatus");
    console.log("status after edit:", status);
    await page.click("#saveConfigButton");
    try {
      await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 20000 });
    } catch (error) {
      console.log("save failed, status:", await page.textContent("#settingsStatus"));
      throw error;
    }
    const config = await (await fetch(`${BASE}/api/config`)).json();
    console.log("saved tools.max_rounds =", config.config.tools.max_rounds, "(before", before, ")");
    const again = await page.$('#settings-general input[aria-label="最大工具轮数"], #settings-general input[aria-label="工具最大轮数"]');
    await again.fill(String(before));
    await sleep(200);
    await page.click("#saveConfigButton");
    await page.waitForFunction(() => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"), null, { timeout: 20000 });
    const restored = await (await fetch(`${BASE}/api/config`)).json();
    console.log("restored tools.max_rounds =", restored.config.tools.max_rounds);
  } else {
    console.log("max_rounds input not found");
  }

  await browser.close();
  console.log("\nERRORS:", errors.length);
  for (const error of errors) console.log(" -", error);
  process.exit(errors.length ? 1 : 0);
})().catch((error) => { console.error("FAILED:", error); process.exit(2); });
