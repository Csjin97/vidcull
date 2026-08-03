import { describe, expect, it } from "vitest";
import { classifyFreshness } from "./freshness.js";

describe("classifyFreshness", () => {
  it("is fresh when the stamp SHA exactly matches HEAD", () => {
    const result = classifyFreshness({
      embeddedStamp: "abc1234 1700000000",
      headSha: "abc1234",
      exeExists: true,
    });
    expect(result).toEqual({ fresh: true, code: "FRESH", dirty: false });
  });

  it("is STALE when the stamp SHA differs from HEAD", () => {
    const result = classifyFreshness({
      embeddedStamp: "deadbee 1700000000",
      headSha: "abc1234",
      exeExists: true,
    });
    expect(result.fresh).toBe(false);
    expect(result.code).toBe("STALE");
    expect(result.dirty).toBe(false);
    expect(result.reason).toContain("deadbee");
    expect(result.reason).toContain("abc1234");
    expect(result.reason).toContain("stage-daemon.ps1");
  });

  it("is MISSING when the daemon exe is not staged", () => {
    const result = classifyFreshness({
      embeddedStamp: "abc1234 1700000000",
      headSha: "abc1234",
      exeExists: false,
    });
    expect(result.fresh).toBe(false);
    expect(result.code).toBe("MISSING");
    expect(result.reason).toContain("app/src-tauri/binaries/");
  });

  it("is NO_GIT when HEAD cannot be resolved", () => {
    const result = classifyFreshness({
      embeddedStamp: "abc1234 1700000000",
      headSha: "",
      exeExists: true,
    });
    expect(result.fresh).toBe(false);
    expect(result.code).toBe("NO_GIT");
  });

  it("is UNKNOWN when the daemon was built without git", () => {
    const result = classifyFreshness({
      embeddedStamp: "unknown 1700000000",
      headSha: "abc1234",
      exeExists: true,
    });
    expect(result.fresh).toBe(false);
    expect(result.code).toBe("UNKNOWN");
    expect(result.reason).toContain("unknown");
  });

  it("is fresh (with dirty=true) when the base SHA matches HEAD despite -dirty suffix", () => {
    const result = classifyFreshness({
      embeddedStamp: "abc1234-dirty 1700000000",
      headSha: "abc1234",
      exeExists: true,
    });
    expect(result).toEqual({ fresh: true, code: "FRESH", dirty: true });
  });
});
