
import { chromium } from "playwright";

const baseUrl = process.argv[2] ?? "http://localhost:4173";
const shot = process.argv[3] ?? "playground-verify.png";
const url = `${baseUrl}/playground`;

const EXPECTED = {
  primaryHex: "#da291c", 
  primaryRgb: "rgb(218, 41, 28)",
  colorSwatches: 26,
  typeRows: 13,
};

const browser = await chromium.launch({ headless: false });
const page = await browser.newPage({ viewport: { width: 1280, height: 1600 } });

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(String(e)));

const failures = [];
function check(name, ok, detail) {
  if (ok) {
    console.log(`  PASS  ${name}`);
  } else {
    failures.push(`${name} — ${detail}`);
    console.log(`  FAIL  ${name} — ${detail}`);
  }
}

await page.goto(url, { waitUntil: "networkidle" });
await page.waitForSelector(".pg-swatch");

const primaryVar = (
  await page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue("--color-primary"),
  )
).trim();
check(
  "--color-primary resolves to spec hex",
  primaryVar === EXPECTED.primaryHex,
  `got "${primaryVar}"`,
);

const firstChipBg = await page.evaluate(() => {
  const chip = document.querySelector(".pg-swatch__chip");
  return chip ? getComputedStyle(chip).backgroundColor : "<none>";
});
check(
  "primary swatch paints Rosso Corsa",
  firstChipBg === EXPECTED.primaryRgb,
  `got "${firstChipBg}"`,
);

const swatchCount = await page.locator(".pg-swatch").count();
check(
  "all colour swatches render",
  swatchCount === EXPECTED.colorSwatches,
  `got ${swatchCount}`,
);
const typeRows = await page.locator(".pg-type__row").count();
check("all typography specimens render", typeRows === EXPECTED.typeRows, `got ${typeRows}`);

const ctaRadius = await page.evaluate(() => {
  const btn = document.querySelector(".btn--primary");
  return btn ? getComputedStyle(btn).borderRadius : "<none>";
});
check("primary CTA keeps 0px corners", ctaRadius === "0px", `got "${ctaRadius}"`);

const badgeRadius = await page.evaluate(() => {
  const badge = document.querySelector(".badge");
  return badge ? getComputedStyle(badge).borderRadius : "<none>";
});
check("badge keeps pill radius", badgeRadius === "9999px", `got "${badgeRadius}"`);

check("no console/page errors", consoleErrors.length === 0, consoleErrors.join(" | "));

await page.screenshot({ path: shot, fullPage: true });
console.log(`\nScreenshot: ${shot}`);

await browser.close();

if (failures.length > 0) {
  console.error(`\n${failures.length} check(s) failed.`);
  process.exit(1);
}
console.log("\nAll playground checks passed.");
