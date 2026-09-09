// 圆角审计：把某个面板里所有元素「实际生效」的 border-radius 枚举出来。
// 静态查 CSS 只能看到写下的规则，这里看的是浏览器算完的结果——SVG 属性、
// 内联样式、被覆盖的规则都跑不掉。
const { chromium } = require("playwright");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const BASE = process.argv[2] || "http://127.0.0.1:8513";
const PANEL = process.argv[3] || "usage";

(async () => {
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1440, height: 1400 } });
  await page.goto(BASE, { waitUntil: "networkidle" });
  await page.click("#consoleButton");
  await page.waitForSelector("#consoleView:not([hidden])");
  await sleep(900);
  await page.click(`.con-rail-item[data-console-panel="${PANEL}"]`, { force: true });
  await page.waitForSelector(`.con-panel[data-console-panel="${PANEL}"]:not([hidden])`);
  await sleep(1600);
  // 统计页默认只看一天，切到「至今」才会把柱状图/环形图/表格铺开。
  // 选择器必须限定在当前面板内：consoleView 里还挂着其它面板的隐藏 DOM，
  // 点到隐藏按钮会直接卡到超时。
  if (PANEL === "usage") {
    for (const btn of await page.$$(`.con-panel[data-console-panel="${PANEL}"] .con-segmented button`)) {
      if ((await btn.textContent()).includes("至今")) { await btn.click(); break; }
    }
    await sleep(1800);
  }

  const rows = await page.evaluate((panel) => {
    const root = document.querySelector(`.con-panel[data-console-panel="${panel}"]`);
    if (!root) return [];
    const seen = new Map();
    for (const node of root.querySelectorAll("*")) {
      const cs = getComputedStyle(node);
      const radius = cs.borderRadius;
      if (!radius || radius === "0px") continue;
      const rect = node.getBoundingClientRect();
      if (!rect.width || !rect.height) continue;
      const cls = typeof node.className === "string"
        ? node.className
        : (node.className && node.className.baseVal) || "";
      const key = `${radius}|${node.tagName.toLowerCase()}.${cls.split(" ")[0]}`;
      if (!seen.has(key)) {
        seen.set(key, {
          radius,
          sel: `${node.tagName.toLowerCase()}.${cls.split(" ")[0]}`,
          size: `${Math.round(rect.width)}×${Math.round(rect.height)}`,
          count: 0,
        });
      }
      seen.get(key).count += 1;
    }
    return [...seen.values()].sort((a, b) => a.radius.localeCompare(b.radius));
  }, PANEL);

  for (const r of rows) {
    console.log(r.radius.padEnd(24), r.sel.padEnd(32), r.size.padEnd(11), "×" + r.count);
  }
  await browser.close();
})();
