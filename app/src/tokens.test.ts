import { describe, expect, it } from "vitest";
import {
  expectedCssVariables,
  loadCssVariables,
  loadDesignTokens,
} from "./design-md";
import {
  COLOR_TOKENS,
  RADIUS_TOKENS,
  SPACING_TOKENS,
  TYPOGRAPHY_TOKENS,
} from "./tokens";


const design = loadDesignTokens();
const expected = expectedCssVariables(design);
const actual = loadCssVariables();

describe("design tokens ↔ docs/design-system.md", () => {
  it("declares every docs/design-system.md token as a CSS variable with the exact value", () => {
    const mismatches: string[] = [];
    for (const [name, value] of expected) {
      const got = actual.get(name);
      if (got !== value) {
        mismatches.push(`${name}: expected "${value}", got "${got ?? "<missing>"}"`);
      }
    }
    expect(mismatches).toEqual([]);
  });

  it("locks the canonical Rosso Corsa to the spec value (#da291c, not #ff2800)", () => {
    expect(actual.get("--color-primary")).toBe("#da291c");
    expect(design.colors.primary).toBe("#da291c");
  });

  it("uses Inter as the documented FerrariSans substitute", () => {
    const fontSans = actual.get("--font-sans");
    expect(fontSans).toBeDefined();
    expect(fontSans).toMatch(/^Inter/);
    expect(actual.get("--font-weight-display")).toBe("500");
  });

  it("keeps the default corner radius sharp (0px) with pill reserved for badges", () => {
    expect(actual.get("--radius-none")).toBe("0px");
    expect(actual.get("--radius-full")).toBe("9999px");
  });
});

describe("token manifest covers the docs/design-system.md spec", () => {
  it("matches the colour token set", () => {
    expect([...COLOR_TOKENS].sort()).toEqual(Object.keys(design.colors).sort());
  });

  it("matches the spacing token set", () => {
    expect([...SPACING_TOKENS].sort()).toEqual(Object.keys(design.spacing).sort());
  });

  it("matches the radius token set", () => {
    expect([...RADIUS_TOKENS].sort()).toEqual(Object.keys(design.rounded).sort());
  });

  it("matches the typography token set", () => {
    expect([...TYPOGRAPHY_TOKENS].sort()).toEqual(
      Object.keys(design.typography).sort(),
    );
  });
});
