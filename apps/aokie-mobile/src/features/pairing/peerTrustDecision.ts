import type { DesktopPeerTrustChallenge, RealtimeConfig } from "../../bridge/CompanionBridge";

export interface PeerTrustDecisionSteps {
  /** Tells the native side the owner's answer to this exact challenge. */
  confirm(challenge: DesktopPeerTrustChallenge, approved: boolean): Promise<void>;
  /** Stops the connection (the native reconnect loop with it) and returns to setup. */
  disconnect(): Promise<void>;
  /** Closes the comparison before a fresh connection may raise another. */
  clear(): void;
  /** Starts the connection again, now that the key is trusted. */
  reconnect(config: RealtimeConfig): Promise<void>;
}

/**
 * What the owner's answer to a Desktop key comparison does.
 *
 * A rejected key ends the connection: the native connection retries on its own
 * (backing off to every 16 s, realtime_v2.rs), each retry met the same untrusted key with a fresh
 * challenge, and the comparison the owner had just refused came back, again and
 * again, until the app restarted. Rejected, the connection is stopped even if
 * telling the native side fails. Approved, the comparison closes first and then
 * the connection starts again, so a challenge that fresh connection raises is
 * not closed with this one.
 */
export async function settlePeerTrust(
  steps: PeerTrustDecisionSteps,
  challenge: DesktopPeerTrustChallenge,
  approved: boolean,
  retry: RealtimeConfig | null,
): Promise<void> {
  if (!approved) {
    try {
      await steps.confirm(challenge, false);
    } finally {
      await steps.disconnect();
    }
    return;
  }
  await steps.confirm(challenge, true);
  steps.clear();
  if (retry) await steps.reconnect(retry);
}
