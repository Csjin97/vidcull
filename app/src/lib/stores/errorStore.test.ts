import { beforeEach, describe, expect, it } from "vitest";
import { errorStore } from "./errorStore.svelte";

beforeEach(() => {
  errorStore.dismissAll();
});

describe("errorStore", () => {

  it("adds an entry when reportError is called", () => {
    errorStore.reportError(new Error("데몬 연결 실패"));
    expect(errorStore.errors).toHaveLength(1);
    expect(errorStore.errors[0].message).toBe("데몬 연결 실패");
    expect(errorStore.errors[0].severity).toBe("error");
    expect(errorStore.errors[0].count).toBe(1);
  });

  it("accumulates multiple distinct messages", () => {
    errorStore.reportError(new Error("오류 A"));
    errorStore.reportError(new Error("오류 B"));
    expect(errorStore.errors).toHaveLength(2);
  });

  it("normalises null input to '알 수 없는 오류'", () => {
    errorStore.reportError(null);
    expect(errorStore.errors[0].message).toBe("알 수 없는 오류");
  });

  it("normalises a plain string", () => {
    errorStore.reportError("IPC 타임아웃");
    expect(errorStore.errors[0].message).toBe("IPC 타임아웃");
  });


  it("increments count for a duplicate message instead of adding a new entry", () => {
    errorStore.reportError(new Error("같은 오류"));
    errorStore.reportError(new Error("같은 오류"));
    errorStore.reportError(new Error("같은 오류"));
    expect(errorStore.errors).toHaveLength(1);
    expect(errorStore.errors[0].count).toBe(3);
  });

  it("treats distinct messages as separate entries", () => {
    errorStore.reportError(new Error("오류 A"));
    errorStore.reportError(new Error("오류 B"));
    errorStore.reportError(new Error("오류 A")); 
    expect(errorStore.errors).toHaveLength(2);
    const a = errorStore.errors.find((e) => e.message === "오류 A")!;
    expect(a.count).toBe(2);
    const b = errorStore.errors.find((e) => e.message === "오류 B")!;
    expect(b.count).toBe(1);
  });


  it("dismiss removes the entry with the given id", () => {
    errorStore.reportError(new Error("삭제될 오류"));
    const id = errorStore.errors[0].id;
    errorStore.dismiss(id);
    expect(errorStore.errors).toHaveLength(0);
  });

  it("dismiss only removes the targeted entry when multiple exist", () => {
    errorStore.reportError(new Error("오류 1"));
    errorStore.reportError(new Error("오류 2"));
    const id = errorStore.errors[0].id;
    errorStore.dismiss(id);
    expect(errorStore.errors).toHaveLength(1);
    expect(errorStore.errors[0].message).toBe("오류 2");
  });

  it("dismiss is a no-op for an unknown id", () => {
    errorStore.reportError(new Error("남아있는 오류"));
    errorStore.dismiss(99999);
    expect(errorStore.errors).toHaveLength(1);
  });


  it("dismissAll clears all entries", () => {
    errorStore.reportError(new Error("오류 A"));
    errorStore.reportError(new Error("오류 B"));
    errorStore.dismissAll();
    expect(errorStore.errors).toHaveLength(0);
  });

  it("dismissAll on an empty store is safe", () => {
    expect(() => errorStore.dismissAll()).not.toThrow();
    expect(errorStore.errors).toHaveLength(0);
  });


  it("evicts the oldest entry when the cap (10) is exceeded", () => {
    for (let i = 0; i < 11; i++) {
      errorStore.reportError(new Error(`오류 ${i}`));
    }
    expect(errorStore.errors).toHaveLength(10);
    expect(errorStore.errors.find((e) => e.message === "오류 0")).toBeUndefined();
    expect(
      errorStore.errors.find((e) => e.message === "오류 10"),
    ).toBeDefined();
  });

  it("never exceeds the cap regardless of how many errors are reported", () => {
    for (let i = 0; i < 25; i++) {
      errorStore.reportError(new Error(`오류 ${i}`));
    }
    expect(errorStore.errors.length).toBeLessThanOrEqual(10);
  });


  it("assigns unique ids to each entry", () => {
    errorStore.reportError(new Error("A"));
    errorStore.reportError(new Error("B"));
    const [a, b] = errorStore.errors;
    expect(a.id).not.toBe(b.id);
  });
});
