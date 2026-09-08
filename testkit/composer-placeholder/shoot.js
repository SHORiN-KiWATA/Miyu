// 浏览器侧验收:输入框的 placeholder 真的被 applyPersona 换成了人格的值。
//
// API 那半由 run.py 钉住;这里钉的是最后一跳——后端返回对了但前端没把它
// 写进 textarea,用户看到的仍是 index.html 里那句写死的兜底。
//
//   MIYU_CPH_PORT=18396 node testkit/composer-placeholder/shoot.js
const { chromium } = require("playwright");

const PORT = process.env.MIYU_CPH_PORT || "18396";

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await page.goto(`http://127.0.0.1:${PORT}/`, { waitUntil: "networkidle" });
  await page.waitForTimeout(1200);

  const seen = await page.evaluate(() => ({
    placeholder: document.getElementById("composerInput")?.placeholder,
    personaName: document.getElementById("brandName")?.textContent,
  }));
  const expected = process.env.MIYU_CPH_EXPECT || `给 ${seen.personaName} 发消息`;

  console.log(`人格名: ${JSON.stringify(seen.personaName)}`);
  console.log(`placeholder: ${JSON.stringify(seen.placeholder)}`);
  console.log(`期望: ${JSON.stringify(expected)}`);

  await browser.close();
  if (seen.placeholder !== expected) {
    console.log("FAIL placeholder 没换成人格的值");
    process.exit(1);
  }
  console.log("全过");
})();
