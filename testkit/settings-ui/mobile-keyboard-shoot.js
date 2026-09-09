// 软键盘弹出时 app-shell 会不会跟着可视视口一起缩。
//
// iOS Safari 不认 viewport meta 的 interactive-widget=resizes-content,键盘弹
// 出时布局视口一动不动、只有可视视口变矮。原来 .app-shell 写死 100dvh,于是外
// 壳比可见区域高出一个键盘;Safari 为了把输入框顶进视野会滚动文档,滚完正好停
// 在外壳底部那片没人画的空白上——表现就是整页上移、满屏纯黑。
//
// 无头 Chromium 变不出真键盘,这里改写 visualViewport.height 再派发 resize,
// 走的是页面自己的 syncAppHeight() 那条真实代码路径。能证的是「外壳跟不跟可
// 视视口」这一半;iOS 那半(浏览器自己的 scroll-into-view 偏移)只能上真机看。
//
// 用法:node mobile-keyboard-shoot.js <baseUrl> <styles.css 路径> <outDir>
const { chromium } = require("playwright");
const path = require("path");
const fs = require("fs");

const BASE = process.argv[2] || "http://127.0.0.1:8300";
const CSS = process.argv[3];
const OUT = process.argv[4] || path.join(__dirname, "keyboard-shots");
const W = Number(process.env.W || 390);
const H = Number(process.env.H || 844);
const KEYBOARD = Number(process.env.KEYBOARD || 514); // iPhone 中文键盘约这么高

fs.mkdirSync(OUT, { recursive: true });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const browser = await chromium.launch({ headless: true });
  const ctx = await browser.newContext({
    viewport: { width: W, height: H },
    deviceScaleFactor: 2,
    isMobile: true,
    hasTouch: true,
    userAgent:
      "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
  });
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
  await sleep(900);

  const measure = () =>
    page.evaluate(() => {
      const box = (selector) => {
        const node = document.querySelector(selector);
        if (!node) return null;
        const rect = node.getBoundingClientRect();
        return { top: Math.round(rect.top), bottom: Math.round(rect.bottom), h: Math.round(rect.height) };
      };
      return {
        visual: Math.round(window.visualViewport.height),
        appHeight:
          getComputedStyle(document.documentElement).getPropertyValue("--app-height").trim() || "(未设)",
        shell: box(".app-shell"),
        stage: box(".main-stage"),
        composer: box(".composer-dock"),
      };
    });

  const before = await measure();
  console.log("键盘弹出前", JSON.stringify(before));
  await page.screenshot({ path: path.join(OUT, "0-before.png") });

  // 造一个软键盘:改写 visualViewport.height,再发它自己的 resize。
  await page.evaluate((keyboard) => {
    const viewport = window.visualViewport;
    const shrunk = viewport.height - keyboard;
    Object.defineProperty(viewport, "height", { configurable: true, get: () => shrunk });
    viewport.dispatchEvent(new Event("resize"));
  }, KEYBOARD);
  await sleep(400);

  const after = await measure();
  console.log("键盘弹出后", JSON.stringify(after));
  await page.screenshot({ path: path.join(OUT, "1-keyboard.png") });

  await browser.close();
  console.log(JSON.stringify({ errors }, null, 2));

  const visible = after.visual;
  const dead = after.shell.h - visible;
  console.log(`可视视口 ${visible}px, 外壳 ${after.shell.h}px, 外壳伸到可视区外 ${dead}px`);
  if (dead > 1) {
    console.error(`FAIL 外壳比可视视口高出 ${dead}px —— Safari 会滚到这片空白上`);
    process.exit(1);
  }
  console.log("PASS 外壳跟随可视视口");
})().catch((e) => {
  console.error("ERR", e.message);
  process.exit(2);
});
