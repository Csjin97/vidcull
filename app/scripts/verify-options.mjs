
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const baseUrl = process.argv[2] ?? "http://localhost:5173";
const shotDir = process.argv[3] ?? "options-verify";
mkdirSync(shotDir, { recursive: true });

const browser = await chromium.launch({ headless: false });
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() !== "error") return;
  const url = m.location()?.url ?? "";
  if (url.includes("favicon") || m.text().includes("favicon")) return;
  consoleErrors.push(`${m.text()} @ ${url}`);
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

await page.goto(`${baseUrl}/options`, { waitUntil: "networkidle" });
await page.evaluate(() => localStorage.removeItem("vidcull.settings"));
await page.reload({ waitUntil: "networkidle" });
await page.waitForSelector(".options", { timeout: 15_000 });

await check("options page renders the form", async () => {
  const n = await page.locator(".opt-card").count();
  return n >= 3 ? true : `got ${n} option cards`;
});

await check("adding a scan folder appends it to the list", async () => {
  await page.locator('input[placeholder="예: D:/videos"]').fill("D:/clips");
  await page.locator(".opt-card").first().locator("button", { hasText: "추가" }).click();
  await page.waitForTimeout(100);
  const txt = (await page.locator(".opt-list").first().textContent()) ?? "";
  return txt.includes("D:/clips") ? true : `list = "${txt}"`;
});

await check("adding an exclude rule appends it", async () => {
  await page.locator('input[placeholder="예: .trash, node_modules"]').fill("node_modules");
  await page
    .locator(".opt-card", { hasText: "폴더명 제외 규칙" })
    .locator("button", { hasText: "추가" })
    .click();
  await page.waitForTimeout(100);
  const body = (await page.locator("body").textContent()) ?? "";
  return body.includes("node_modules") ? true : "exclude rule not shown";
});

await check("toggling start-on-boot flips the checkbox", async () => {
  const boot = page
    .locator(".opt-toggle", { hasText: "시스템 시작 시 자동 실행" })
    .locator('input[type="checkbox"]');
  await boot.check();
  return (await boot.isChecked()) ? true : "checkbox did not check";
});

await check("saving shows the saved status", async () => {
  await page.waitForSelector('[data-testid="save-status"]', { timeout: 5_000 });
  const txt = (await page.locator('[data-testid="save-status"]').textContent()) ?? "";
  return txt.includes("저장됨") ? true : `status = "${txt}"`;
});

await check("settings persist to localStorage", async () => {
  const raw = await page.evaluate(() =>
    localStorage.getItem("vidcull.settings"),
  );
  if (!raw) return "nothing persisted";
  const parsed = JSON.parse(raw);
  return parsed.scanFolders?.includes("D:/clips") && parsed.runOnBoot === true
    ? true
    : `persisted = ${raw}`;
});

await page.screenshot({ path: `${shotDir}/01-options.png`, fullPage: true });

await check("reloading restores the saved settings", async () => {
  await page.reload({ waitUntil: "networkidle" });
  await page.waitForSelector(".options", { timeout: 10_000 });
  const body = (await page.locator("body").textContent()) ?? "";
  const boot = page
    .locator(".opt-toggle", { hasText: "시스템 시작 시 자동 실행" })
    .locator('input[type="checkbox"]');
  return body.includes("D:/clips") && (await boot.isChecked())
    ? true
    : "saved settings not restored after reload";
});

await check("main page progress panel starts expanded", async () => {
  await page.goto(baseUrl, { waitUntil: "networkidle" });
  await page.waitForSelector('[data-testid="progress-panel"]', { timeout: 15_000 });
  return (await page.locator('[data-testid="progress-metrics"]').count()) > 0
    ? true
    : "panel not expanded on load";
});

await check("clicking the toggle collapses the progress panel", async () => {
  const panel = page.locator('[data-testid="progress-panel"]');
  await page.locator('[data-testid="progress-toggle"]').click();
  await page.waitForTimeout(150);
  const collapsed = await panel.evaluate((el) =>
    el.classList.contains("progress--collapsed"),
  );
  if (!collapsed) return "panel did not collapse after toggle";
  return (await page.locator('[data-testid="progress-metrics"]').count()) === 0
    ? true
    : "chart still visible while collapsed";
});

await check("clicking the toggle again expands it", async () => {
  await page.locator('[data-testid="progress-toggle"]').click();
  await page.waitForTimeout(150);
  const collapsed = await page
    .locator('[data-testid="progress-panel"]')
    .evaluate((el) => el.classList.contains("progress--collapsed"));
  return collapsed ? "panel stayed collapsed" : true;
});

await page.screenshot({ path: `${shotDir}/02-collapsed.png`, fullPage: false });

await check("no console / page errors during the flow", () =>
  consoleErrors.length === 0 ? true : consoleErrors.join(" | "),
);

await browser.close();

console.log(`\nScreenshots in ${shotDir}/`);
if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed.`);
  process.exit(1);
}
console.log("\nAll options + progress-collapse checks passed.");
