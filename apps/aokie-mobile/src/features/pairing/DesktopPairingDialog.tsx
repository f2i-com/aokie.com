import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Check,
  CheckCircle2,
  Clipboard,
  FileUp,
  KeyRound,
  LockKeyhole,
  MonitorSmartphone,
  RefreshCw,
  Server,
  ShieldCheck,
  Smartphone,
  Timer,
  X,
} from "lucide-react";
import type {
  CompanionBridge,
  DesktopPairingDecision,
  DesktopPairingReview,
  ManagedAdmissionState,
  ServerProfile,
} from "../../bridge";
import { displayError } from "../../utils/displayError";

const MAX_PAIRING_FILE_BYTES = 16 * 1024;

export type DesktopPairingStep = 1 | 2 | 3;

export function desktopPairingStep(
  review: DesktopPairingReview | null,
  decision: DesktopPairingDecision | null,
): DesktopPairingStep {
  if (decision?.responseJson) return 3;
  return review ? 2 : 1;
}

export function pairingConfirmationReady(
  review: DesktopPairingReview | null,
  desktopCompared: boolean,
  mobileAcknowledged: boolean,
  displayName: string,
  nowSeconds: number,
): boolean {
  return Boolean(
    review && desktopCompared && mobileAcknowledged && displayName.trim() &&
    displayName.length <= 120 && review.expiresAt > nowSeconds,
  );
}

export default function DesktopPairingDialog({
  bridge,
  admission,
  retrying,
  onClose,
  onRetry,
}: {
  bridge: CompanionBridge;
  admission: ManagedAdmissionState | null;
  retrying: boolean;
  onClose(): void;
  onRetry(): Promise<void>;
}) {
  const [profiles, setProfiles] = useState<ServerProfile[]>([]);
  const [profilesLoading, setProfilesLoading] = useState(true);
  const [offerJson, setOfferJson] = useState("");
  const [displayName, setDisplayName] = useState("Aokie Companion");
  const [review, setReview] = useState<DesktopPairingReview | null>(null);
  const [decision, setDecision] = useState<DesktopPairingDecision | null>(null);
  const [desktopCompared, setDesktopCompared] = useState(false);
  const [mobileAcknowledged, setMobileAcknowledged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1_000));
  const activeProfile = useMemo(() => profiles.find((profile) => profile.active) ?? null, [profiles]);

  useEffect(() => {
    let cancelled = false;
    setProfilesLoading(true);
    void bridge.listServerProfiles()
      .then((loaded) => { if (!cancelled) setProfiles(loaded); })
      .catch((caught) => { if (!cancelled) setError(displayError(caught, "The active server profile could not be loaded")); })
      .finally(() => { if (!cancelled) setProfilesLoading(false); });
    return () => { cancelled = true; };
  }, [bridge]);

  useEffect(() => {
    if (!review || decision) return;
    const timer = window.setInterval(() => setNow(Math.floor(Date.now() / 1_000)), 1_000);
    return () => window.clearInterval(timer);
  }, [decision, review]);

  const remaining = review ? Math.max(0, review.expiresAt - now) : 0;
  const canSign = pairingConfirmationReady(review, desktopCompared, mobileAcknowledged, displayName, now);
  const currentStep = desktopPairingStep(review, decision);
  const expiryState = remaining <= 0 ? "is-expired" : remaining <= 30 ? "is-urgent" : "";

  const reviewOffer = async () => {
    if (!activeProfile || busy) return;
    setBusy(true);
    setError(null);
    setCopied(false);
    try {
      const next = await bridge.reviewDesktopPairingOffer(activeProfile.profileId, offerJson);
      if (next.profileId !== activeProfile.profileId || next.appId !== activeProfile.appId || next.deviceId !== activeProfile.deviceId) {
        throw new Error("Native pairing review crossed the selected server-profile binding");
      }
      setReview(next);
      setDecision(null);
      setDesktopCompared(false);
      setMobileAcknowledged(false);
      setNow(Math.floor(Date.now() / 1_000));
    } catch (caught) {
      setError(displayError(caught, "The Desktop pairing offer could not be reviewed"));
    } finally {
      setBusy(false);
    }
  };

  const decide = async (approved: boolean) => {
    if (!review || busy) return;
    setBusy(true);
    setError(null);
    try {
      const result = await bridge.confirmDesktopPairing(review, {
        displayName,
        approved,
        desktopThumbprintConfirmed: approved ? desktopCompared : false,
        mobileFingerprintAcknowledged: approved ? mobileAcknowledged : false,
      });
      if (approved) setDecision(result);
      else {
        setReview(null);
        setDecision(null);
        setDesktopCompared(false);
        setMobileAcknowledged(false);
      }
    } catch (caught) {
      setError(displayError(caught, "The Desktop pairing decision could not be completed"));
    } finally {
      setBusy(false);
    }
  };

  const importOffer = async (file: File | undefined) => {
    if (!file) return;
    setError(null);
    if (file.size < 1 || file.size > MAX_PAIRING_FILE_BYTES) {
      setError("Pairing offer files must be between 1 byte and 16 KiB.");
      return;
    }
    const text = await file.text();
    if (!text.trim() || text.length > MAX_PAIRING_FILE_BYTES) {
      setError("Pairing offer files must contain at most 16 KiB of JSON.");
      return;
    }
    setOfferJson(text);
  };

  const copyResponse = async () => {
    if (!decision?.responseJson) return;
    try {
      await navigator.clipboard.writeText(decision.responseJson);
      setCopied(true);
      setError(null);
    } catch (caught) {
      setError(displayError(caught, "The response could not be copied; select the JSON manually"));
    }
  };

  const close = () => {
    if (busy || retrying) return;
    if (review && !decision) void decide(false).then(onClose);
    else onClose();
  };

  return (
    <div className="modal-layer is-centered desktop-pairing-modal" role="presentation">
      <section
        className="modal-sheet is-dialog desktop-pairing-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="desktop-pairing-title"
        aria-describedby="desktop-pairing-description"
      >
        <button className="icon-button pairing-close" aria-label="Close Desktop pairing" disabled={busy || retrying} onClick={close}><X size={19} /></button>

        <header className="pairing-header">
          <div className="pairing-header-icon" aria-hidden="true"><LockKeyhole size={24} /></div>
          <div className="pairing-header-copy">
            <span className="section-kicker">OWNER-CONFIRMED PAIRING</span>
            <h2 id="desktop-pairing-title">Pair with Aokie Desktop</h2>
            <p id="desktop-pairing-description">Create a private, pinned connection by comparing both devices before either one is trusted.</p>
          </div>
        </header>

        <ol className="pairing-progress" aria-label="Desktop pairing progress">
          {[
            { step: 1 as const, label: "Add offer", hint: "From Desktop" },
            { step: 2 as const, label: "Verify keys", hint: "Compare both" },
            { step: 3 as const, label: "Approve", hint: "Back in Desktop" },
          ].map((item) => {
            const complete = currentStep > item.step;
            const active = currentStep === item.step;
            return (
              <li key={item.step} className={`${complete ? "is-complete" : ""} ${active ? "is-active" : ""}`} aria-current={active ? "step" : undefined}>
                <span className="pairing-step-number" aria-hidden="true">{complete ? <Check size={14} /> : item.step}</span>
                <span className="pairing-step-copy"><strong>{item.label}</strong><small>{item.hint}</small></span>
              </li>
            );
          })}
        </ol>

        {admission && <div className={`setup-result pairing-admission ${admission.value === "desktop_unavailable" ? "is-warning" : "is-failed"}`} role="status"><AlertTriangle size={17} /><span>{admission.message}</span></div>}

        {!decision && <>
          {profilesLoading ? <p className="setup-blocked-copy pairing-loading"><RefreshCw className="spin" size={14} /> Loading the active native profile…</p> : activeProfile ? (
            <section className="pairing-profile" aria-labelledby="pairing-profile-title">
              <span className="pairing-profile-icon" aria-hidden="true"><Server size={19} /></span>
              <div className="pairing-profile-main">
                <span id="pairing-profile-title">Pairing through active server</span>
                <strong>{activeProfile.origin}</strong>
                <small>{activeProfile.deploymentId}</small>
              </div>
              <dl className="pairing-profile-bindings">
                <div><dt>App</dt><dd title={activeProfile.appId}>{activeProfile.appId}</dd></div>
                <div><dt>Device</dt><dd title={activeProfile.deviceId}>{activeProfile.deviceId}</dd></div>
              </dl>
            </section>
          ) : <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>Authorize or select an active server profile before pairing. Native code will not invent a device ID or pair across profiles.</span></div>}

          {!review ? <>
            <div className="pairing-stage-heading">
              <span className="pairing-stage-icon" aria-hidden="true"><MonitorSmartphone size={19} /></span>
              <div><strong>Bring the one-use offer from Desktop</strong><p>Paste the JSON shown by Aokie Desktop, or import the saved offer file.</p></div>
            </div>
            <label className="setup-field pairing-offer-field">
              <span>Desktop one-use JSON offer</span>
              <textarea rows={7} value={offerJson} onChange={(event) => setOfferJson(event.target.value)} placeholder='Paste the {"kind":"aokie_mobile_pairing", ...} offer from Desktop' autoCapitalize="none" autoCorrect="off" spellCheck={false} />
              <small className="pairing-field-help"><LockKeyhole size={13} /> The offer is validated by native code before any confirmation is shown.</small>
            </label>
            <div className="pairing-import-row">
              <label className="secondary-button pairing-file-button"><FileUp size={17} /> Import .json or .txt<input type="file" accept=".json,.txt,application/json,text/plain" onChange={(event) => void importOffer(event.target.files?.[0])} /></label>
              <button className="primary-button" disabled={busy || !activeProfile || !offerJson.trim()} onClick={() => void reviewOffer()}>{busy ? <RefreshCw className="spin" size={17} /> : <ShieldCheck size={17} />}{busy ? "Validating natively…" : "Review public offer"}</button>
            </div>
            <aside className="pairing-security-note"><AlertTriangle size={18} /><p><strong>This offer does not prove Desktop’s identity.</strong> Anyone can copy public binding data. The next step asks you to compare the complete thumbprint with Desktop through a trusted, separate view.</p></aside>
          </> : <>
            <div className="pairing-review-heading">
              <div className="pairing-stage-heading">
                <span className="pairing-stage-icon" aria-hidden="true"><KeyRound size={19} /></span>
                <div><strong>Compare both device identities</strong><p>Match every character. Similar-looking or shortened values are not enough.</p></div>
              </div>
              <div className={`pairing-expiry ${expiryState}`} title="One-use offer lifetime">
                <Timer size={15} aria-hidden="true" />
                <span>{remaining > 0 ? `${remaining}s left` : "Expired"}</span>
              </div>
            </div>

            <div className="pairing-identity-grid">
              <section className="pairing-identity-card is-desktop" aria-labelledby="desktop-identity-title">
                <header>
                  <span className="pairing-identity-icon" aria-hidden="true"><MonitorSmartphone size={19} /></span>
                  <div><span>Desktop identity</span><strong id="desktop-identity-title">Compare on Aokie Desktop</strong></div>
                  <span className="pairing-identity-badge">Compare</span>
                </header>
                <div className="pairing-fingerprint-value is-primary">
                  <span>Protocol thumbprint — every character</span>
                  <code tabIndex={0} aria-label="Desktop protocol thumbprint">{review.desktopKeyThumbprint}</code>
                </div>
                <div className="pairing-fingerprint-value">
                  <span>Raw-key fingerprint</span>
                  <code tabIndex={0} aria-label="Desktop raw-key fingerprint">{review.desktopFingerprint}</code>
                </div>
              </section>

              <section className="pairing-identity-card is-companion" aria-labelledby="companion-identity-title">
                <header>
                  <span className="pairing-identity-icon" aria-hidden="true"><Smartphone size={19} /></span>
                  <div><span>This device</span><strong id="companion-identity-title">Companion identity</strong></div>
                  <span className="pairing-identity-badge">Show Desktop</span>
                </header>
                <div className="pairing-fingerprint-value is-primary">
                  <span>Fingerprint Desktop must show</span>
                  <code tabIndex={0} aria-label="Companion fingerprint">{review.mobileFingerprint}</code>
                </div>
                <div className="pairing-fingerprint-value">
                  <span>Protocol thumbprint</span>
                  <code tabIndex={0} aria-label="Companion protocol thumbprint">{review.mobileKeyThumbprint}</code>
                </div>
              </section>
            </div>

            <section className="pairing-confirm-zone" aria-labelledby="pairing-confirm-title">
              <header>
                <span aria-hidden="true"><ShieldCheck size={19} /></span>
                <div><strong id="pairing-confirm-title">Owner confirmation</strong><p>Both checks are required before this device can sign a response.</p></div>
              </header>
              <label className={`pairing-confirmation ${desktopCompared ? "is-checked" : ""}`}>
                <input type="checkbox" checked={desktopCompared} onChange={(event) => setDesktopCompared(event.target.checked)} />
                <span><strong>Desktop thumbprint matches</strong><small>I compared every character of the full Desktop protocol thumbprint with the value shown by Aokie Desktop.</small></span>
              </label>
              <label className={`pairing-confirmation ${mobileAcknowledged ? "is-checked" : ""}`}>
                <input type="checkbox" checked={mobileAcknowledged} onChange={(event) => setMobileAcknowledged(event.target.checked)} />
                <span><strong>I recognize this Companion</strong><small>I will compare every character of its fingerprint with Desktop’s pending approval before approving there.</small></span>
              </label>
            </section>

            <label className="setup-field pairing-display-name"><span>Companion display name</span><input value={displayName} maxLength={120} onChange={(event) => setDisplayName(event.target.value)} /><small className="pairing-field-help">This is how the device will appear in Desktop’s pending approval.</small></label>
            {remaining <= 0 && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>The offer expired. Reject this review and create a fresh offer in Desktop.</span></div>}
            <div className="pairing-decision-actions">
              <button className="primary-button" disabled={busy || !canSign} onClick={() => void decide(true)}>{busy ? <RefreshCw className="spin" size={17} /> : <LockKeyhole size={17} />}{busy ? "Signing natively…" : "Confirm exact keys and sign response"}</button>
              <button className="pairing-reject-button" disabled={busy} onClick={() => void decide(false)}><X size={16} /> Reject offer and start over</button>
            </div>
          </>}
        </>}

        {decision?.responseJson && <>
          <div className="pairing-success-heading" role="status">
            <span aria-hidden="true"><CheckCircle2 size={22} /></span>
            <div><strong>Signed response ready</strong><p>No private key or OAuth credential entered the WebView. Desktop still needs your local approval.</p></div>
          </div>
          <section className="pairing-identity-card is-companion is-compact" aria-labelledby="response-fingerprint-title">
            <header>
              <span className="pairing-identity-icon" aria-hidden="true"><Smartphone size={19} /></span>
              <div><span>Final identity check</span><strong id="response-fingerprint-title">Companion fingerprint Desktop must show</strong></div>
            </header>
            <div className="pairing-fingerprint-value is-primary"><code tabIndex={0}>{decision.mobileFingerprint}</code></div>
          </section>
          <label className="setup-field pairing-response-field"><span>Signed response JSON — paste into Desktop</span><textarea rows={10} readOnly value={decision.responseJson} onFocus={(event) => event.currentTarget.select()} /></label>
          <button className="primary-button" onClick={() => void copyResponse()}><Clipboard size={17} />{copied ? "Response copied" : "Copy response JSON"}</button>
          <div className="pairing-next-steps"><strong>Finish in Aokie Desktop</strong><ol><li><span>1</span><p>Paste this signed response.</p></li><li><span>2</span><p>Compare the full Companion fingerprint.</p></li><li><span>3</span><p>Approve the pending mobile endpoint locally.</p></li><li><span>4</span><p>Return here and retry the connection.</p></li></ol></div>
          <button className="secondary-button" disabled={retrying} onClick={() => void onRetry()}>{retrying ? <RefreshCw className="spin" size={17} /> : <RefreshCw size={17} />}{retrying ? "Waiting for native admission…" : "Retry connection after Desktop approval"}</button>
        </>}

        {error && <div className="setup-result is-failed" role="alert"><AlertTriangle size={17} /><span>{error}</span></div>}
      </section>
    </div>
  );
}
