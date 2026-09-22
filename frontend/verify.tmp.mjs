import puppeteer from "puppeteer-core";

const browser = await puppeteer.launch({
  executablePath: "/usr/bin/chromium",
  args: ["--no-sandbox", "--disable-gpu"],
});
const page = await browser.newPage();
await page.setViewport({ width: 1600, height: 900 });
const errs = [];
page.on("pageerror", (e) => errs.push(String(e).slice(0, 200)));
await page.goto("http://localhost:15176/", { waitUntil: "networkidle0", timeout: 30000 });
await page.waitForSelector("canvas", { timeout: 15000 });
await new Promise((r) => setTimeout(r, 3000));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// --- 1. shift test: click on a bar center, overlay must sit under the cursor
const box = await page.evaluate(() => {
  const el = document.querySelector(".chartwrap .chart");
  const r = el.getBoundingClientRect();
  return { x: r.x, y: r.y, w: r.width, h: r.height };
});
const tools = await page.$$(".drawbar .tool-btn");
console.log("tools:", tools.length); // ray, zone, position, ruler + trash = 5
await tools[1].click(); // zone
const zx = box.x + box.w * 0.3;
const zy1 = box.y + box.h * 0.3;
await page.mouse.click(zx, zy1); await sleep(400);
await page.mouse.click(box.x + box.w * 0.45, box.y + box.h * 0.6); await sleep(600);
const zoneLeft = await page.evaluate(() => {
  const z = document.querySelector(".ov-zone:not(.ghost)");
  return z ? parseFloat(z.style.left) : null;
});
console.log("zone click x rel:", (zx - box.x).toFixed(1), "rendered left:", zoneLeft);
// NOTE: snapped to bar center, tolerance = half bar width; just report.

// --- 2. market BUY blocked without stop, then allowed with stop
const tradeCard = () => page.evaluateHandle(() => {
  const cards = [...document.querySelectorAll(".card")];
  return cards.find((c) => c.textContent.includes("Вход по рынку"));
});
const buyBtn = () => page.evaluate(() => {
  const cards = [...document.querySelectorAll(".card")];
  const t = cards.find((c) => c.textContent.includes("Вход по рынку"));
  const btns = [...t.querySelectorAll(".btn")];
  return btns.find((b) => b.textContent === "BUY").disabled;
});
console.log("buy disabled w/o stop:", await buyBtn());
const last = await page.evaluate(() => {
  const el = [...document.querySelectorAll(".card")].find((c) => c.querySelector("h4")?.textContent === "Market");
  const m = el?.textContent.match(/last ([\d,]+\.\d+)/);
  return m ? Number(m[1].replace(/,/g, "")) : null;
});
console.log("last price:", last);
const sl = (last - 500).toFixed(1);
const stopInput = await page.evaluateHandle(() => {
  const cards = [...document.querySelectorAll(".card")];
  const t = cards.find((c) => c.textContent.includes("Вход по рынку"));
  return t.querySelector('input[placeholder="цена стопа"]');
});
await stopInput.click({ clickCount: 3 });
await stopInput.type(sl);
await sleep(400);
console.log("buy disabled with stop:", await buyBtn());
console.log("preview:", await page.evaluate(() => {
  const cards = [...document.querySelectorAll(".card")];
  const t = cards.find((c) => c.textContent.includes("Вход по рынку"));
  const st = [...t.querySelectorAll(".status")];
  return st.length ? st[0].textContent : "none";
}));
// place the order
await page.evaluate(() => {
  const cards = [...document.querySelectorAll(".card")];
  const t = cards.find((c) => c.textContent.includes("Вход по рынку"));
  [...t.querySelectorAll(".btn")].find((b) => b.textContent === "BUY").click();
});
await sleep(2500);
// NOTE: this trades on the shared :9001 engine; read back position value
const posTab = await page.evaluate(() => {
  const tabs = [...document.querySelectorAll(".bottom-terminal .tab")];
  const p = tabs.find((t) => t.textContent.includes("Positions"));
  if (p) p.click();
  return true;
});
await sleep(600);
const posBody = await page.evaluate(() => document.querySelector(".bottom-terminal .tab-body")?.textContent.slice(0, 200) ?? "none");
console.log("positions tab:", JSON.stringify(posBody));
console.log("ERRORS:", errs.length ? errs : "none");
await browser.close();
