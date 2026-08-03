
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const baseUrl = process.argv[2] ?? "http://localhost:5173";
const shotDir = process.argv[3] ?? "review-verify";
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

await check("content-cluster cards render", async () => {
  const n = await page.locator(".card").count();
  return n > 0 ? true : `got ${n} cards`;
});

await check("cluster-list header total is populated", async () => {
  const txt = (await page.locator(".review__total").first().textContent()) ?? "";
  return /\d+개 그룹/.test(txt) ? true : `got "${txt}"`;
});

await check("a cluster card shows EXACT + VERY_LIKELY badges together", async () => {
  const firstCard = page.locator(".card").first();
  const badges = await firstCard.locator(".badge").allTextContents();
  const hasExact = badges.some((t) => t.includes("완전 동일"));
  const hasVeryLikely = badges.some((t) => t.includes("유사 (재인코딩)"));
  return hasExact && hasVeryLikely
    ? true
    : `badges on first card = ${JSON.stringify(badges)}`;
});

await check("thumbnails paint (mock SVG previews)", async () => {
  const imgs = page.locator(".card .thumb__img");
  const n = await imgs.count();
  if (n === 0) return "no .thumb__img rendered";
  const src = await imgs.first().getAttribute("src");
  return src && src.startsWith("data:image/") ? true : `first src = "${src}"`;
});

await check("reclaimable-space header renders", async () => {
  const body = (await page.locator("body").textContent()) ?? "";
  return /회수|GB|MB|KB|B\b/.test(body) ? true : "no reclaimable figure found";
});

await check("trust filter tab switches the list", async () => {
  const exactTab = page.locator(".tab", { hasText: "완전 동일" });
  await exactTab.click();
  await page.waitForTimeout(200);
  const active = await page
    .locator(".tab--active")
    .first()
    .textContent();
  return active?.includes("완전 동일") ? true : `active tab = "${active}"`;
});
await page.locator(".tab", { hasText: "전체" }).click();
await page.waitForTimeout(200);

await page.screenshot({ path: `${shotDir}/01-list.png`, fullPage: false });

await check("selecting a cluster opens the compare view", async () => {
  await page.locator(".card").first().click();
  await page.waitForSelector(".compare", { timeout: 5_000 });
  return true;
});

await check("compare view lists member specs", async () => {
  const members = await page.locator(".compare .member").count();
  return members >= 2 ? true : `got ${members} members`;
});

await check("member name is not vertically stacked (UI_IDEA bug)", async () => {
  const box = await page.locator(".compare .member__name").first().boundingBox();
  if (!box) return "no .member__name box";
  return box.height < 48
    ? true
    : `name height ${box.height}px suggests vertical wrapping`;
});

await check("list card titles stay single-line in split layout", async () => {
  const titles = page.locator(".review__list .card__title");
  const n = Math.min(await titles.count(), 6);
  for (let i = 0; i < n; i += 1) {
    const box = await titles.nth(i).boundingBox();
    if (box && box.height > 40) {
      return `card title ${i} height ${box.height}px — wrapping vertically`;
    }
  }
  return true;
});

await page.screenshot({ path: `${shotDir}/02-compare.png`, fullPage: false });

await check("delete button opens the confirm dialog", async () => {
  await page.locator(".compare__summary button").click();
  await page.waitForSelector(".dlg__content", { timeout: 5_000 });
  return true;
});

await check("dialog states the delete/keep counts", async () => {
  const desc = (await page.locator(".dlg__desc").textContent()) ?? "";
  return /\d+개 파일을 삭제하고\s*\d+개를 보존/.test(desc)
    ? true
    : `got "${desc}"`;
});

await page.screenshot({ path: `${shotDir}/03-dialog.png`, fullPage: false });

await check("confirming the deletion closes the dialog", async () => {
  const confirm = page.locator(".dlg__actions button").last();
  await confirm.click();
  await page.waitForSelector(".dlg__content", { state: "detached", timeout: 5_000 });
  return true;
});

await check("options route renders", async () => {
  await page.goto(`${baseUrl}/options`, { waitUntil: "networkidle" });
  const body = (await page.locator("body").textContent()) ?? "";
  return body.length > 0 ? true : "empty options page";
});
await page.screenshot({ path: `${shotDir}/04-options.png`, fullPage: true });

await check("no console / page errors during the flow", () =>
  consoleErrors.length === 0 ? true : consoleErrors.join(" | "),
);

await browser.close();

console.log(`\nScreenshots in ${shotDir}/`);
if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed.`);
  process.exit(1);
}
console.log("\nAll review-UI checks passed.");
