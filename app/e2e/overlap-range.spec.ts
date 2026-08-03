import { test, expect } from "@playwright/test";


const RANGE = /\d+:\d{2}\s*[–-]\s*\d+:\d{2}/; 

test("POSSIBLE cluster compare view shows overlap time ranges", async ({
  page,
}) => {
  await page.goto("/", { waitUntil: "networkidle" });
  await page.waitForSelector(".card", { timeout: 15_000 });

  await page.locator(".tab", { hasText: "유사 (추정)" }).click();
  await page.waitForTimeout(200);
  await page.locator(".card").first().click();

  const timeline = page.locator('[data-testid="overlap-timeline"]');
  await expect(timeline).toBeVisible({ timeout: 5_000 });
  await expect(timeline).toBeInViewport();

  const ranges = page.locator('[data-testid="overlap-range"]');
  await expect(ranges.first()).toBeVisible();
  expect(await ranges.count()).toBeGreaterThan(0);
  const text = (await ranges.first().textContent()) ?? "";
  expect(text, `overlap range row text was "${text}"`).toMatch(RANGE);
});

test("clicking a non-default member focuses it (red border) and reframes the overlap range", async ({
  page,
}) => {
  await page.goto("/", { waitUntil: "networkidle" });
  await page.waitForSelector(".card", { timeout: 15_000 });

  await page.locator(".tab", { hasText: "유사 (추정)" }).click();
  await page.waitForTimeout(200);
  await page.locator(".card").first().click();

  const timeline = page.locator('[data-testid="overlap-timeline"]');
  await expect(timeline).toBeVisible({ timeout: 5_000 });
  await expect(timeline).toBeInViewport();

  const ranges = page.locator('[data-testid="overlap-range"]');
  await expect(ranges.first()).toBeVisible();
  const sourceRangeText = (await ranges.first().textContent()) ?? "";
  expect(sourceRangeText).toMatch(RANGE);

  const focusButtons = page.locator(".member__focus");
  const memberCount = await focusButtons.count();
  expect(memberCount).toBeGreaterThan(1);

  let reframed = false;
  for (let i = 0; i < memberCount; i += 1) {
    await focusButtons.nth(i).click();
    await expect(page.locator(".member--focused")).toHaveCount(1);
    const text = (await ranges.first().textContent()) ?? "";
    expect(text).toMatch(RANGE);
    if (text !== sourceRangeText) {
      reframed = true;
      break;
    }
  }
  expect(
    reframed,
    "clicking the clip member must reframe the overlap range to its own timeline",
  ).toBe(true);
});
