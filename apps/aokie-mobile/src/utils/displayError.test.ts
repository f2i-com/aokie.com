import { describe, expect, it } from "vitest";
import { displayError } from "./displayError";

describe("displayError", () => {
  it("preserves a non-empty Tauri string rejection", () => {
    expect(displayError("  schema-v2 discovery contains an unknown field  ", "Discovery failed"))
      .toBe("schema-v2 discovery contains an unknown field");
  });

  it("preserves JavaScript Error and serialized message rejections", () => {
    expect(displayError(new Error("native profile is no longer trusted"), "Profile failed"))
      .toBe("native profile is no longer trusted");
    expect(displayError({ message: "OAuth authorization expired", code: "expired" }, "Authorization failed"))
      .toBe("OAuth authorization expired");
  });

  it("uses the contextual fallback for empty or opaque rejections", () => {
    expect(displayError("   ", "Discovery failed")).toBe("Discovery failed");
    expect(displayError({ code: "unknown" }, "Discovery failed")).toBe("Discovery failed");
    expect(displayError(null, "Discovery failed")).toBe("Discovery failed");
  });

  it("does not invoke arbitrary coercion or message accessors", () => {
    const rejection = {
      get message(): string { throw new Error("must not run"); },
      toString(): string { throw new Error("must not run"); },
    };
    expect(displayError(rejection, "Native operation failed")).toBe("Native operation failed");
  });

  it("bounds unexpectedly large native diagnostics", () => {
    const message = "x".repeat(5_000);
    const result = displayError(message, "Native operation failed");
    expect(result).toHaveLength(4_096);
    expect(result.endsWith("…")).toBe(true);
  });
});
