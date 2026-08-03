import "@testing-library/jest-dom/vitest";

class ObserverStub {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
  takeRecords(): [] {
    return [];
  }
}

if (!("IntersectionObserver" in globalThis)) {
  globalThis.IntersectionObserver = ObserverStub;
}
if (!("ResizeObserver" in globalThis)) {
  globalThis.ResizeObserver = ObserverStub;
}
