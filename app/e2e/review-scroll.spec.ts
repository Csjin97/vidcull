import { test, expect, type Locator } from "@playwright/test";


const VLIST = ".review__scroll .vlist";

async function metrics(vlist: Locator) {
  return vlist.evaluate((el) => ({
    clientHeight: el.clientHeight,
    scrollHeight: el.scrollHeight,
    scrollTop: el.scrollTop,
    overflowY: getComputedStyle(el).overflowY,
  }));
}

test.describe("duplicate-review list scrolling", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/", { waitUntil: "networkidle" });
    await page.waitForSelector(".card", { timeout: 15_000 });
  });

  test("the list viewport is scrollable (content overflows a bounded height)", async ({
    page,
  }) => {
    const vlist = page.locator(VLIST);
    await expect(vlist).toBeVisible();
    const m = await metrics(vlist);

    expect(m.clientHeight, "vlist clientHeight should be > 0").toBeGreaterThan(0);
    expect(m.overflowY, "vlist overflow-y should allow scrolling").toMatch(
      /auto|scroll/,
    );
    expect(
      m.scrollHeight,
      "content (120 rows × 140px) should overflow the bounded viewport",
    ).toBeGreaterThan(m.clientHeight + 100);
  });

  test("scrolling the list advances scrollTop and renders later rows", async ({
    page,
  }) => {
    const vlist = page.locator(VLIST);
    const before = await metrics(vlist);

    const firstTitleBefore = await page
      .locator(`${VLIST} .card__title`)
      .first()
      .textContent();

    await vlist.hover();
    await page.mouse.wheel(0, before.clientHeight * 3);
    await page.waitForTimeout(150);

    const afterWheel = await metrics(vlist);
    expect(
      afterWheel.scrollTop,
      "wheel scroll should move scrollTop",
    ).toBeGreaterThan(before.scrollTop);

    await vlist.evaluate((el) => {
      el.scrollTop = el.scrollHeight - el.clientHeight;
    });
    await page.waitForTimeout(150);

    const atEnd = await metrics(vlist);
    expect(atEnd.scrollTop).toBeGreaterThan(afterWheel.scrollTop);

    const firstTitleAfter = await page
      .locator(`${VLIST} .card__title`)
      .first()
      .textContent();
    expect(
      firstTitleAfter,
      "the windowed rows should change after scrolling to the end",
    ).not.toBe(firstTitleBefore);
  });
});
