import puppeteer from "puppeteer-core";

const browser = await puppeteer.launch({
  executablePath: "/usr/bin/chromium",
  args: ["--no-sandbox", "--disable-gpu"],
});
const page = await browser.newPage();
await page.setViewport({ width: 1600, height: 900 });
const errs = [];
page.on("pageerror", (e) => errs.push(String(e).slice(0, 200)));
await page.goto("http://localhost:15177/", { waitUntil: "domcontentloaded", timeout: 30000 });
await page.waitForSelector("canvas", { timeout: 15000 });
await new Promise((r) => setTimeout(r, 3000));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fedText = () => page.evaluate(() => {
  const cards = [...document.querySelectorAll(".card")];
  const r = cards.find((c) => c.textContent.includes("REPLAY") || c.textContent.includes("Replay"));
  const m = r?.textContent.match(/bar (\d+)\/(\d+)/);
  return m ? { fed: Number(m[1]), total: Number(m[2]) } : null;
});
const box = await page.evaluate(() => {
  const el = document.querySelector(".chartwrap .chart");
  const r = el.getBoundingClientRect();
  return { x: r.x, y: r.y, w: r.width, h: r.height };
});
console.log("before barrier:", JSON.stringify(await fedText()));
const tools = await page.$$(".drawbar .tool-btn");
console.log("tools:", tools.length);
await tools[1].click(); // vline
await sleep(300);
await page.mouse.click(box.x + box.w * 0.3, box.y + box.h * 0.5);
await sleep(1500);
const after = await fedText();
console.log("after barrier:", JSON.stringify(after));
console.log("vlines:", await page.evaluate(() => document.querySelectorAll(".ov-vline").length));
await page.screenshot({ path: "/tmp/w_barrier.png" });
// play -> bars appear one by one
await page.evaluate(() => {
  const btns = [...document.querySelectorAll(".card .btn")];
  const p = btns.find((b) => b.textContent === "▶");
  if (p) p.click();
});
await sleep(1000);
const f1 = await fedText();
await sleep(4000);
const f2 = await fedText();
console.log("playing:", JSON.stringify(f1), "->", JSON.stringify(f2));
console.log("grew one-by-one:", f2 && f1 && f2.fed > f1.fed && f2.fed - f1.fed < 20 ? "OK" : "CHECK");
await page.screenshot({ path: "/tmp/w_reveal.png" });
console.log("ERRORS:", errs.length ? errs : "none");
await browser.close();
