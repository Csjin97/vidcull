
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const baseUrl = process.argv[2] ?? "http://localhost:5180";
const shotDir = process.argv[3] ?? "scenarios-verify";
mkdirSync(shotDir, { recursive: true });

const browser = await chromium.launch({ headless: false });
const page = await browser.newPage({ viewport: { width: 1440, height: 960 } });

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(String(e)));

const failures = [];
async function check(name, fn) {
  try {
    const detail = await fn();
    if (detail === true || detail === undefined) {
      console.log(`  PASS  ${name}`);
    } else {
      failures.push(`${name} — ${detail}`);
      console.log(`  FAIL  ${name} — ${detail}`);
    }
  } catch (err) {
    failures.push(`${name} — threw ${err}`);
    console.log(`  FAIL  ${name} — threw ${err}`);
  }
}

await page.goto(baseUrl, { waitUntil: "networkidle" });
await page.waitForSelector(".card", { timeout: 15_000 });

await check("S10 — sidebar status shows offline in dev (no daemon)", async () => {
  const dot = page.locator(".sidebar__status .status-dot");
  if ((await dot.count()) === 0) return "no status dot rendered";
  const isOk = await dot.first().evaluate((el) => el.classList.contains("status--ok"));
  if (isOk) return "status dot is 'ok' but no daemon should be reachable in dev";
  const versionShown = await page.locator(".sidebar__status-version").count();
  return versionShown === 0 ? true : "version pill shown while offline";
});

await check("S5 — POSSIBLE trust tab yields partial-clip clusters", async () => {
  await page.locator(".tab", { hasText: "유사 (추정)" }).click();
  await page.waitForTimeout(250);
  const n = await page.locator(".card").count();
  return n > 0 ? true : "no POSSIBLE clusters after filtering";
});

await check("S5 — selecting a partial-clip cluster opens the compare view", async () => {
  await page.locator(".card").first().click();
  await page.waitForSelector(".compare", { timeout: 5_000 });
  return true;
});

await check("S5 — the overlap timeline renders for a partial-clip cluster", async () => {
  const tl = page.locator('[data-testid="overlap-timeline"]');
  await tl.waitFor({ state: "visible", timeout: 5_000 });
  const head = (await tl.locator(".eyebrow").textContent()) ?? "";
  return head.includes("부분 클립 겹침") ? true : `timeline head = "${head}"`;
});

await check("S5 — the timeline draws at least one clip overlap bar", async () => {
  const bars = await page.locator('[data-testid="overlap-bar"]').count();
  return bars >= 1 ? true : `got ${bars} overlap bars`;
});

await page.screenshot({ path: `${shotDir}/02-overlap.png`, fullPage: false });

await check("S8 — failure accordion toggle is present with a count", async () => {
  await page.goto(baseUrl, { waitUntil: "networkidle" });
  const toggle = page.locator(".failures__toggle");
  await toggle.waitFor({ state: "visible", timeout: 10_000 });
  const title = (await page.locator(".failures__title").textContent()) ?? "";
  return /실패\s*\d+건/.test(title) ? true : `title = "${title}"`;
});

await check("S8 — opening the accordion lists the failed files", async () => {
  await page.locator(".failures__toggle").click();
  await page.waitForSelector(".failures--open", { timeout: 5_000 });
  const rows = await page.locator(".failures__row").count();
  if (rows < 1) return `got ${rows} failure rows`;
  const body = (await page.locator(".failures__list").textContent()) ?? "";
  return /corrupt-finale\.mkv/.test(body) && /old-codec\.avi/.test(body)
    ? true
    : `list text = "${body.slice(0, 160)}"`;
});

await page.screenshot({ path: `${shotDir}/03-failures.png`, fullPage: false });

await check("no console / page errors during the flow", () =>
  consoleErrors.length === 0 ? true : consoleErrors.join(" | "),
);

await browser.close();

console.log(`\nScreenshots in ${shotDir}/`);
if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed.`);
  process.exit(1);
}
console.log("\nAll scenario checks passed.");
