/** What `document` offers that matters here, so a test can stand in for it. */
export interface VisibilityTarget {
  readonly visibilityState: string;
  addEventListener(type: "visibilitychange", listener: () => void): void;
  removeEventListener(type: "visibilitychange", listener: () => void): void;
}

/**
 * Runs `refresh` each time the page becomes visible again, and returns the function that stops it.
 *
 * Android changes what the app may do while it is in the background: the user can grant or take away
 * the notification permission in Settings and come back. Nothing in the web view hears about that, so
 * the readiness tiles and the "notifications are denied" notice kept showing what was true when the
 * app started until it was relaunched. A refresh that is still running is not started a second time,
 * and one that fails is dropped: the next return to the app asks again.
 */
export function refreshWhenForegrounded(
  target: VisibilityTarget,
  refresh: () => Promise<unknown>,
): () => void {
  let running = false;
  const onVisibilityChange = () => {
    if (target.visibilityState !== "visible" || running) return;
    running = true;
    void refresh()
      .catch(() => undefined)
      .finally(() => {
        running = false;
      });
  };
  target.addEventListener("visibilitychange", onVisibilityChange);
  return () => target.removeEventListener("visibilitychange", onVisibilityChange);
}
