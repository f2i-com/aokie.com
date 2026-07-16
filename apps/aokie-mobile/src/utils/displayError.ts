const MAX_DISPLAY_ERROR_LENGTH = 4_096;

/**
 * Turn a native/async rejection into safe text for a React error surface.
 *
 * Tauri commands that return `Result<_, String>` reject with the Rust string
 * itself, rather than with a JavaScript `Error`. Do not stringify arbitrary
 * rejection objects: that can invoke user-defined coercion and can expose an
 * unhelpful `[object Object]` in the UI.
 */
export function displayError(caught: unknown, fallback: string): string {
  let candidate: string | undefined;

  try {
    if (typeof caught === "string") candidate = caught;
    else if (caught instanceof Error) candidate = caught.message;
    else if (isErrorMessageRecord(caught)) candidate = caught.message;
  } catch {
    return fallback;
  }

  const normalized = candidate?.trim();
  if (!normalized) return fallback;
  if (normalized.length <= MAX_DISPLAY_ERROR_LENGTH) return normalized;
  return `${normalized.slice(0, MAX_DISPLAY_ERROR_LENGTH - 1)}…`;
}

function isErrorMessageRecord(value: unknown): value is { message: string } {
  if (typeof value !== "object" || value === null) return false;
  const descriptor = Object.getOwnPropertyDescriptor(value, "message");
  return Boolean(descriptor && "value" in descriptor && typeof descriptor.value === "string");
}
