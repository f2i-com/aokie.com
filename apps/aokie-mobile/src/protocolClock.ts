/**
 * Protocol time in the renderer: this phone's clock moved onto the server's.
 *
 * Native code reads the server's clock from the `Date` of each response that carries an endpoint challenge,
 * and checks every lease, offer, consent and assistance time against it. The renderer gates its own controls
 * (whether Take over is offered, an assistance or End-caller countdown) on the same server-clock times, so it
 * uses the same offset: with its own clock alone, a phone a few seconds out hid a control native code would
 * have accepted. Times native code stamps on this phone's own clock (native media expiry, local proofs,
 * trust dialogs) keep using `Date.now()`.
 */

/** The largest offset accepted, matching native code's own bound. */
export const MAX_PROTOCOL_CLOCK_OFFSET_SECONDS = 60 * 60;

let offsetMilliseconds = 0;

/** Take the offset native code sends with each snapshot. Anything malformed leaves the current one. */
export function setProtocolClockOffsetSeconds(seconds: unknown): void {
  if (
    typeof seconds !== "number" ||
    !Number.isSafeInteger(seconds) ||
    Math.abs(seconds) > MAX_PROTOCOL_CLOCK_OFFSET_SECONDS
  ) {
    return;
  }
  offsetMilliseconds = seconds * 1_000;
}

/** Milliseconds on the server's clock. */
export function protocolNowMs(): number {
  return Date.now() + offsetMilliseconds;
}

/** Whole seconds on the server's clock. */
export function protocolNowSeconds(): number {
  return Math.floor(protocolNowMs() / 1_000);
}
