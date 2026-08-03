import { beforeEach, describe, expect, it } from "vitest";
import { introOutroVisibilityStore } from "./introOutroVisibilityStore.svelte";

beforeEach(() => {
  introOutroVisibilityStore.set(false);
});

describe("introOutroVisibilityStore", () => {
  it("defaults to hidden (shown === false)", () => {
    expect(introOutroVisibilityStore.shown).toBe(false);
  });

  it("set(true) reveals, set(false) hides", () => {
    introOutroVisibilityStore.set(true);
    expect(introOutroVisibilityStore.shown).toBe(true);
    introOutroVisibilityStore.set(false);
    expect(introOutroVisibilityStore.shown).toBe(false);
  });

  it("toggle() flips the current value", () => {
    expect(introOutroVisibilityStore.shown).toBe(false);
    introOutroVisibilityStore.toggle();
    expect(introOutroVisibilityStore.shown).toBe(true);
    introOutroVisibilityStore.toggle();
    expect(introOutroVisibilityStore.shown).toBe(false);
  });
});
