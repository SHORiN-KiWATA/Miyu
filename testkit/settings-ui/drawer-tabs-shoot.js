// 插件设置抽屉的标签条 A/B:内容一长时标签条会不会被挤扁。
//
// 抽屉是列向 flex(.st-drawer),标签条 .st-tabs 自己带 overflow-y: hidden,
// 于是它的自动最小尺寸是 0——不锁 flex-shrink 就会被长内容按基准尺寸加权摊
// 掉高度,里面 min-height:38px 的按钮被自己的 overflow 从下面裁掉。
//
// 用法:node drawer-tabs-shoot.js <baseUrl> <styles.css 路径> <outDir>
// 跑两遍(修复前后的 styles.css 各一遍)对比 tabsHeight。
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:8300";
const CSS = process.argv[3];
const OUT = process.argv[4] || path.join(__dirname, "drawer-shots");
const PLUGIN = process.env.PLUGIN || "群聊真实上下文回复";
const TAB = process.env.TAB || "主动回复判断";

fs.mkdirSync(OUT, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(`pageerror: ${e.message}`));

  if (CSS) {
    const body = fs.readFileSync(CSS, "utf8");
    await page.route("**/styles.css*", (route) =>
      route.fulfill({ status: 200, contentType: "text/css; charset=utf-8", body })
    );
  }

  await page.goto(BASE, { waitUntil: "networkidle" });
  await sleep(600);

  await page.click("#consoleButton");
  await page.waitForSelector("#consoleView:not([hidden])");
  await page.click('.con-rail-item[data-console-panel="settings"]', { force: true });
  await page.waitForSelector('[data-console-panel="settings"]:not([hidden])');
  try {
    await page.waitForFunction(
      () => document.getElementById("settingsStatus")?.textContent?.includes("配置已同步"),
      null,
      { timeout: 20000 }
    );
  } catch (_) {}
  await page.click('[data-settings-view="qq"]', { force: true });
  await sleep(600);

  // 卡片没有 id 属性,只能按标题找;而且「插件」页也有一批同名 class 的卡片藏
  // 在隐藏视图里,所以要挑真正在布局里的那张(高度不为 0)。
  const opened = await page.evaluate((title) => {
    for (const open of document.querySelectorAll(".st-plugin-open")) {
      if (open.getBoundingClientRect().height === 0) continue;
      if (open.querySelector("strong")?.textContent.trim() === title) {
        open.click();
        return true;
      }
    }
    return false;
  }, PLUGIN);
  if (!opened) throw new Error(`找不到插件卡片「${PLUGIN}」`);
  await page.waitForSelector(".st-drawer .st-tabs");
  await sleep(500);

  const measure = async (label) => {
    const info = await page.evaluate(() => {
      const bar = document.querySelector(".st-drawer .st-tabs");
      const body = document.querySelector(".st-drawer .st-drawer-body");
      const btn = bar?.querySelector(".st-tab");
      const barBox = bar.getBoundingClientRect();
      const btnBox = btn?.getBoundingClientRect();
      return {
        tabsHeight: Math.round(barBox.height),
        buttonHeight: btnBox ? Math.round(btnBox.height) : 0,
        // 按钮下沿超出标签条下沿多少 px = 被裁掉的量
        clipped: btnBox ? Math.max(0, Math.round(btnBox.bottom - barBox.bottom)) : 0,
        bodyScrollHeight: Math.round(body.scrollHeight),
        bodyHeight: Math.round(body.getBoundingClientRect().height),
      };
    });
    console.log(label, JSON.stringify(info));
    await page.screenshot({ path: path.join(OUT, `${label}.png`) });
    return info;
  };

  const first = await measure("tab-0-first");
  // 切到内容最长的那个标签页
  const clicked = await page.evaluate((label) => {
    for (const b of document.querySelectorAll(".st-drawer .st-tab")) {
      if (b.textContent.trim() === label) {
        b.click();
        return true;
      }
    }
    return false;
  }, TAB);
  if (!clicked) throw new Error(`找不到标签页「${TAB}」`);
  await sleep(700);
  const long = await measure("tab-1-long");

  console.log(JSON.stringify({ errors, first, long }, null, 2));
  await browser.close();
  // 判据:切到长标签页之后标签条不得比按钮矮,按钮也不得被裁。
  if (long.tabsHeight < long.buttonHeight || long.clipped > 0) {
    console.error(`FAIL 标签条被挤扁: 高 ${long.tabsHeight}px, 按钮 ${long.buttonHeight}px, 裁掉 ${long.clipped}px`);
    process.exit(1);
  }
  console.log("PASS 标签条未被挤扁");
})().catch((e) => {
  console.error("ERR", e.message);
  process.exit(2);
});
