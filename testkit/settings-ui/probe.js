const { chromium } = require("playwright");
(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const logs = [];
  page.on("console", (m) => logs.push(m.text()));
  await page.goto("http://127.0.0.1:18391/", { waitUntil: "networkidle" });
  // 标记 DOM 节点,看切换后是否被替换;记录动画数量
  const mark = async (label) => page.evaluate((label) => {
    const nodes = { timeline: document.getElementById("timeline") || document.querySelector(".timeline"), stage: document.querySelector(".main-stage"), settingsRoot: document.getElementById("settings-providers") };
    window.__marks = window.__marks || {};
    window.__marks[label] = Object.fromEntries(Object.entries(nodes).map(([k, n]) => [k, n ? (n.__id ||= Math.random()) : null]));
    const anims = document.getAnimations().filter((a) => a.playState === "running").map((a) => `${a.animationName || a.constructor.name}@${a.effect?.target?.className || "?"}`);
    return { ids: window.__marks[label], running: anims.slice(0, 12), count: anims.length };
  }, label);
  await page.waitForTimeout(800);
  console.log("chat idle:", JSON.stringify(await mark("a")));
  await page.click("#sidebarCollapseButton");
  await page.waitForTimeout(80);
  console.log("after collapse sidebar:", JSON.stringify(await mark("b")));
  await page.click("#sidebarExpandButton");
  await page.waitForTimeout(80);
  console.log("after expand sidebar:", JSON.stringify(await mark("c")));
  await page.click("#sidebarSettingsButton");
  await page.waitForTimeout(1200);
  await page.click('[data-settings-view="providers"]');
  await page.waitForTimeout(900);
  console.log("settings idle:", JSON.stringify(await mark("d")));
  const railBtn = await page.$(".con-rail-head .icon-button");
  await railBtn.click();
  await page.waitForTimeout(80);
  console.log("after rail collapse:", JSON.stringify(await mark("e")));
  await railBtn.click();
  await page.waitForTimeout(80);
  console.log("after rail expand:", JSON.stringify(await mark("f")));
  console.log(logs.filter((l) => !/theme.css|404/.test(l)).slice(0, 5));
  await browser.close();
})();
