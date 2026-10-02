import { describe, expect, it } from "vitest";
import { refreshWhenForegrounded, type VisibilityTarget } from "./foregroundRefresh";

class FakeDocument implements VisibilityTarget {
  visibilityState = "visible";
  private listeners = new Set<() => void>();
  addEventListener(_type: "visibilitychange", listener: () => void) {
    this.listeners.add(listener);
  }
  removeEventListener(_type: "visibilitychange", listener: () => void) {
    this.listeners.delete(listener);
  }
  get listenerCount() {
    return this.listeners.size;
  }
  show() {
    this.visibilityState = "visible";
    for (const listener of [...this.listeners]) listener();
  }
  hide() {
    this.visibilityState = "hidden";
    for (const listener of [...this.listeners]) listener();
  }
}

describe("refreshWhenForegrounded", () => {
  it("refreshes when the page comes back and not when it goes away", async () => {
    const doc = new FakeDocument();
    let calls = 0;
    refreshWhenForegrounded(doc, async () => {
      calls += 1;
    });
    doc.hide();
    expect(calls).toBe(0);
    doc.show();
    expect(calls).toBe(1);
    await new Promise((resolve) => setTimeout(resolve, 0));
    doc.hide();
    doc.show();
    expect(calls).toBe(2);
  });

  it("does not start a second refresh while one is still running", async () => {
    const doc = new FakeDocument();
    let calls = 0;
    let finish: () => void = () => undefined;
    refreshWhenForegrounded(doc, () => {
      calls += 1;
      return new Promise<void>((resolve) => {
        finish = resolve;
      });
    });
    doc.show();
    doc.hide();
    doc.show();
    expect(calls).toBe(1);
    finish();
    await new Promise((resolve) => setTimeout(resolve, 0));
    doc.show();
    expect(calls).toBe(2);
  });

  it("drops a failed refresh and asks again the next time", async () => {
    const doc = new FakeDocument();
    let calls = 0;
    refreshWhenForegrounded(doc, async () => {
      calls += 1;
      throw new Error("native runtime unavailable");
    });
    doc.show();
    await new Promise((resolve) => setTimeout(resolve, 0));
    doc.show();
    expect(calls).toBe(2);
  });

  it("stops listening when it is cleaned up", () => {
    const doc = new FakeDocument();
    let calls = 0;
    const stop = refreshWhenForegrounded(doc, async () => {
      calls += 1;
    });
    expect(doc.listenerCount).toBe(1);
    stop();
    expect(doc.listenerCount).toBe(0);
    doc.show();
    expect(calls).toBe(0);
  });
});
