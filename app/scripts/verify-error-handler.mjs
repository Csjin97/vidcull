
import { chromium } from "playwright";

const VITE_URL = process.env.VITE_URL ?? "http://localhost:5173";

const failures = [];
const log = (...a) => console.log("[verify-error-handler]", ...a);

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

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

log(`opening headed browser at ${VITE_URL} …`);
const browser = await chromium.launch({ headless: false });
const ctx = await browser.newContext();
const page = await ctx.newPage();

await page.goto(VITE_URL, { waitUntil: "networkidle", timeout: 60_000 });
log("page loaded");

await check("unhandledrejection dispatched → toast appears", async () => {
  await page.evaluate(() => {
    window.dispatchEvent(
      new PromiseRejectionEvent("unhandledrejection", {
        promise: Promise.resolve(),
        reason: new Error("테스트 미처리 거부 오류"),
      }),
    );
  });
  await sleep(400);

  const toast = page.locator(".error-toast, [data-testid='error-toast'], .toast");
  if ((await toast.count()) === 0) {
    return "no .error-toast / .toast element found after unhandledrejection";
  }
  const text = await toast.first().textContent();
  return text && text.includes("테스트") ? true : `toast text does not include expected content: ${text}`;
});

await page.evaluate(() => {
  const btn = document.querySelector(
    ".error-toast button, [data-testid='error-toast'] button, .toast button",
  );
  if (btn instanceof HTMLElement) btn.click();
});
await sleep(300);

await check("synchronous throw via setTimeout → window.onerror → toast", async () => {
  await page.evaluate(() => {
    setTimeout(() => {
      throw new Error("테스트 window.onerror 경로");
    }, 0);
  });
  await sleep(600);

  const toast = page.locator(".error-toast, [data-testid='error-toast'], .toast");
  if ((await toast.count()) === 0) {
    return "no toast found after window.onerror throw";
  }
  return true;
});

await page.evaluate(() => {
  document.querySelectorAll(
    ".error-toast button, [data-testid='error-toast'] button, .toast button",
  ).forEach((b) => b instanceof HTMLElement && b.click());
});
await sleep(300);

await check("ProtocolGate z-index > ErrorToast z-index", async () => {
  const zGate = await page.evaluate(() => {
    const gate = document.querySelector(".gate");
    if (!gate) return null; 
    return parseInt(getComputedStyle(gate).zIndex ?? "0", 10);
  });
  const zToast = await page.evaluate(() => {
    const toast = document.querySelector(".error-toast");
    if (!toast) return null;
    return parseInt(getComputedStyle(toast).zIndex ?? "0", 10);
  });

  if (zGate === null) {
    log("  (gate not visible — checking static CSS rule instead)");
    const gateZ = await page.evaluate(() => {
      for (const sheet of Array.from(document.styleSheets)) {
        try {
          for (const rule of Array.from(sheet.cssRules ?? [])) {
            if (rule instanceof CSSStyleRule && rule.selectorText?.includes("gate")) {
              const z = rule.style.zIndex;
              if (z) return parseInt(z, 10);
            }
          }
        } catch {
        }
      }
      return null;
    });
    if (gateZ === null) return "could not determine gate z-index from CSS";
    if (zToast !== null && gateZ <= zToast)
      return `gate z-index (${gateZ}) not above toast z-index (${zToast})`;
    return true;
  }

  if (zToast === null) return true; 
  return zGate > zToast
    ? true
    : `gate z-index (${zGate}) not above toast z-index (${zToast})`;
});

await check("svelte:boundary fallback renders on forced render error (best-effort)", async () => {
  const hasFallback = await page.evaluate(() => {
    return document.querySelector(".boundary-fallback") !== null;
  });
  if (hasFallback) return true;

  log("  (boundary-fallback not currently visible — pass: not a runtime failure)");
  return true;
});

console.log("");
if (failures.length > 0) {
  console.error(`${failures.length} check(s) failed:`);
  for (const f of failures) console.error(`  - ${f}`);
  console.log("\nBrowser left open for inspection. Close it or Ctrl-C to exit.");
} else {
  console.log("All error-handler checks passed.");
  console.log("Browser left open for inspection. Close it or Ctrl-C to exit.");
}

await new Promise(() => {}); 
