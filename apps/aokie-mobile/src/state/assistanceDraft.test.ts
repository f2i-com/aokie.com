import { describe, expect, it } from "vitest";
import { shouldClearAssistanceDraft } from "./assistanceDraft";

describe("assistance draft acknowledgement", () => {
  const submission = { requestId: "request_original", editorRevision: 4 };

  it("clears only an unchanged draft for the exact acknowledged request", () => {
    expect(shouldClearAssistanceDraft(true, submission, "request_original", 4)).toBe(true);
    expect(shouldClearAssistanceDraft(false, submission, "request_original", 4)).toBe(false);
  });

  it("preserves any newer editor revision even when its text is identical", () => {
    expect(shouldClearAssistanceDraft(true, submission, "request_original", 5)).toBe(false);
  });

  it("preserves a draft after the active assistance request changes", () => {
    expect(shouldClearAssistanceDraft(true, submission, "request_replacement", 4)).toBe(false);
    expect(shouldClearAssistanceDraft(true, submission, null, 4)).toBe(false);
  });
});
