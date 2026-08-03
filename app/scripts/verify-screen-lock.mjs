
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const baseUrl = process.argv[2] ?? "http://localhost:5173";
const shotDir = process.argv[3] ?? "screen-lock-verify";
mkdirSync(shotDir, { recursive: true });

const PW = "test1234";
const WRONG = "wrongpass";
const LS_KEY = "vidcull.screenLock";

const browser = await chromium.launch({ headless: false, slowMo: 250 });
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() !== "error") return;
  const url = m.location()?.url ?? "";
  const text = m.text();
  if (url.includes("favicon") || text.includes("favicon")) return;
  if (/tauri|invoke|daemon|__TAURI|fetch failed|ECONN/i.test(text)) return;
  consoleErrors.push(`${text} @ ${url}`);
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

const overlay = () => page.locator('.overlay[role="dialog"]');
const lockCard = () => page.locator(".opt-card", { hasText: "화면 잠금" });

await page.goto(`${baseUrl}/options`, { waitUntil: "networkidle" });
await page.evaluate((k) => {
  localStorage.removeItem(k);
  localStorage.removeItem("vidcull.settings");
}, LS_KEY);
await page.reload({ waitUntil: "networkidle" });
await page.waitForSelector(".options", { timeout: 15_000 });

await check("screen-lock card renders", async () =>
  (await lockCard().count()) === 1 ? true : "card not found",
);

await check("enable toggle is disabled before a password is set", async () => {
  const cb = lockCard().locator('[data-testid="lock-enabled"]');
  return (await cb.isDisabled()) ? true : "toggle was enabled with no password";
});

await check("setting a password (matching confirm) enables the lock", async () => {
  await lockCard().getByPlaceholder("새 비밀번호").fill(PW);
  await lockCard().getByPlaceholder("비밀번호 확인").fill(PW);
  await lockCard().getByRole("button", { name: "비밀번호 설정" }).click();
  await page.waitForTimeout(200);
  const raw = await page.evaluate((k) => localStorage.getItem(k), LS_KEY);
  if (!raw) return "nothing persisted to localStorage";
  const cfg = JSON.parse(raw);
  if (!cfg.enabled) return `enabled !== true: ${raw}`;
  if (!cfg.saltB64 || !cfg.hashB64) return `missing salt/hash: ${raw}`;
  if (raw.includes(PW)) return "plaintext password leaked into storage!";
  return true;
});

await check("mismatched confirm shows an error and does NOT change the password", async () => {
  await lockCard().getByPlaceholder("새 비밀번호").fill("aaaa");
  await lockCard().getByPlaceholder("비밀번호 확인").fill("bbbb");
  await lockCard().getByRole("button", { name: "비밀번호 설정" }).click();
  await page.waitForTimeout(150);
  const err = (await lockCard().locator(".opt-error").textContent()) ?? "";
  return err.includes("일치하지 않") ? true : `error = "${err}"`;
});

await page.screenshot({ path: `${shotDir}/01-options-lock-card.png`, fullPage: true });

await check('"지금 잠금" shows the full-screen overlay', async () => {
  await lockCard().locator('[data-testid="lock-now"]').click();
  await overlay().waitFor({ state: "visible", timeout: 5_000 });
  const z = await overlay().evaluate((el) => getComputedStyle(el).zIndex);
  return z === "999" ? true : `overlay z-index = ${z}`;
});

await page.screenshot({ path: `${shotDir}/02-locked.png`, fullPage: true });

await check("wrong password is rejected and stays locked", async () => {
  await overlay().getByLabel("잠금 해제 비밀번호").fill(WRONG);
  await overlay().getByRole("button", { name: "잠금 해제" }).click();
  await page.waitForTimeout(200);
  const err = (await overlay().locator(".panel__error").textContent()) ?? "";
  const stillLocked = await overlay().isVisible();
  if (!stillLocked) return "overlay dismissed on a WRONG password!";
  return err.includes("틀렸") ? true : `error = "${err}", visible=${stillLocked}`;
});

await page.screenshot({ path: `${shotDir}/03-wrong-password.png`, fullPage: false });

await check("correct password unlocks (overlay disappears)", async () => {
  await overlay().getByLabel("잠금 해제 비밀번호").fill(PW);
  await overlay().getByRole("button", { name: "잠금 해제" }).click();
  await overlay().waitFor({ state: "hidden", timeout: 5_000 });
  return (await overlay().count()) === 0 ? true : "overlay still present after correct password";
});

await check("reload re-locks (lock-on-launch while enabled)", async () => {
  await page.goto(baseUrl, { waitUntil: "networkidle" });
  await overlay().waitFor({ state: "visible", timeout: 8_000 });
  return (await overlay().isVisible()) ? true : "did not lock on reload";
});

await page.screenshot({ path: `${shotDir}/04-relock-on-launch.png`, fullPage: true });

await check("unlock again after reload", async () => {
  await overlay().getByLabel("잠금 해제 비밀번호").fill(PW);
  await overlay().getByRole("button", { name: "잠금 해제" }).click();
  await overlay().waitFor({ state: "hidden", timeout: 5_000 });
  return true;
});

await check("disabling the lock stops it from re-locking on reload", async () => {
  await page.goto(`${baseUrl}/options`, { waitUntil: "networkidle" });
  if (await overlay().isVisible().catch(() => false)) {
    await overlay().getByLabel("잠금 해제 비밀번호").fill(PW);
    await overlay().getByRole("button", { name: "잠금 해제" }).click();
    await overlay().waitFor({ state: "hidden", timeout: 5_000 });
  }
  await page.waitForSelector(".options", { timeout: 10_000 });
  await lockCard().getByRole("button", { name: "비밀번호 해제 (잠금 끄기)" }).click();
  await page.waitForTimeout(150);
  const raw = await page.evaluate((k) => localStorage.getItem(k), LS_KEY);
  const cfg = raw ? JSON.parse(raw) : {};
  if (cfg.enabled) return "still enabled after disable";
  await page.goto(baseUrl, { waitUntil: "networkidle" });
  await page.waitForTimeout(800);
  return (await overlay().count()) === 0 ? true : "overlay appeared though lock is disabled";
});

await check("no unexpected console / page errors during the flow", () =>
  consoleErrors.length === 0 ? true : consoleErrors.join(" | "),
);

await browser.close();

console.log(`\nScreenshots in ${shotDir}/`);
if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed.`);
  process.exit(1);
}
console.log("\nAll screen-lock checks passed.");
