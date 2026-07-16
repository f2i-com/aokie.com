import AVFoundation
import Foundation
import UIKit

/// Implemented by a future native Tauri iOS plugin/Rust bridge. These methods
/// are native-only; do not mirror them into renderer commands or events.
public protocol AokieNativeAuthoritySink: AnyObject {
    func nativeAnswerWasRequested(callID: String, callEpoch: UInt64, offerID: String)
    func nativeCompanionLegEnded(callID: String?, callEpoch: UInt64?, reason: String)
    func nativeAPNsTokenDidRotate(_ token: Data, topic: String)
    func nativeAPNsTokenWasInvalidated()
    func nativeVoIPTokenDidRotate(_ token: Data, topic: String)
    func nativeVoIPTokenWasInvalidated()
    func nativeDiagnostic(_ code: String)
}

/// Source-level iOS composition root. It is not wired into Tauri until an iOS
/// project, plugin registration, entitlements, and Rust FFI adapter are added.
public final class AokieIOSNativeRuntime: NSObject {
    public weak var authoritySink: AokieNativeAuthoritySink?
    public weak var mediaSink: AokieNativeMediaSink? {
        didSet { audio.mediaSink = mediaSink }
    }

    public let credentialVault: AokieCredentialVault
    private let calls: AokieCallCoordinator
    private let audio: AokieAudioSessionController
    private let push: AokiePushRuntime

    public override init() {
        let keychain = AokieKeychainStore()
        let pending = AokiePendingOfferStore(store: keychain)
        let calls = AokieCallCoordinator(store: pending)
        self.credentialVault = AokieCredentialVault(store: keychain)
        self.calls = calls
        self.audio = AokieAudioSessionController()
        self.push = AokiePushRuntime(store: keychain, calls: calls)
        super.init()
        calls.sink = self
        push.sink = self
    }

    /// Call on the main queue after application launch.
    public func startVoIPPushRegistration() {
        dispatchPrecondition(condition: .onQueue(.main))
        push.start()
    }

    /// Call on the main queue from application launch. The AppDelegate must
    /// forward the success/failure callbacks below; no token enters React.
    public func startStandardPushRegistration() {
        dispatchPrecondition(condition: .onQueue(.main))
        UIApplication.shared.registerForRemoteNotifications()
    }

    public func didRegisterForRemoteNotifications(deviceToken: Data) {
        push.didRegisterStandardPushToken(deviceToken)
    }

    public func didFailToRegisterForRemoteNotifications() {
        push.didFailStandardPushRegistration()
    }

    /// Invoke during native logout after the authenticated bridge has sent
    /// DELETE for both APNs endpoint kinds.
    public func invalidatePushTokensForLogout() {
        dispatchPrecondition(condition: .onQueue(.main))
        UIApplication.shared.unregisterForRemoteNotifications()
        push.invalidateAllTokens()
    }

    /// Reconciles the pre-WebView CallKit surface with signed protocol-v2
    /// authority. Returns false for a stale/wrong call epoch.
    @discardableResult
    public func reconcileWinner(callID: String, callEpoch: UInt64) -> Bool {
        dispatchPrecondition(condition: .onQueue(.main))
        return calls.reconcileWinner(callID: callID, callEpoch: callEpoch)
    }

    public func reconcileCancellation(offerID: String, reason: String) {
        calls.cancel(offerID: offerID, reason: reason)
    }
}

extension AokieIOSNativeRuntime: AokieCallCoordinatorSink {
    func callCoordinatorDidRequestAnswer(_ offer: AokieVoiceOffer) {
        authoritySink?.nativeAnswerWasRequested(
            callID: offer.callID,
            callEpoch: offer.callEpoch,
            offerID: offer.offerID
        )
    }

    func callCoordinatorDidGrantAuthority(_ offer: AokieVoiceOffer) {
        do {
            try audio.prepareForIncomingCall()
            audio.grantAuthority()
        } catch {
            audio.revokeAuthority(reason: "audio_session_configuration_failed")
            authoritySink?.nativeDiagnostic("audio_session_configuration_failed")
            calls.cancel(
                offerID: offer.offerID,
                reason: "audio_session_configuration_failed"
            )
        }
    }

    func callCoordinatorDidActivate(_ audioSession: AVAudioSession) {
        audio.callKitDidActivate(audioSession)
    }

    func callCoordinatorDidDeactivate(_ audioSession: AVAudioSession) {
        audio.callKitDidDeactivate(audioSession)
    }

    func callCoordinatorDidEnd(_ offer: AokieVoiceOffer?, reason: String) {
        audio.revokeAuthority(reason: reason)
        authoritySink?.nativeCompanionLegEnded(
            callID: offer?.callID,
            callEpoch: offer?.callEpoch,
            reason: reason
        )
    }

    func callCoordinatorDiagnostic(_ code: String) {
        authoritySink?.nativeDiagnostic(code)
    }
}

extension AokieIOSNativeRuntime: AokiePushRuntimeSink {
    func standardPushTokenDidRotate(_ token: Data) {
        guard let topic = Bundle.main.bundleIdentifier else {
            authoritySink?.nativeDiagnostic("apns_topic_unavailable")
            return
        }
        authoritySink?.nativeAPNsTokenDidRotate(token, topic: topic)
    }

    func standardPushTokenWasInvalidated() {
        authoritySink?.nativeAPNsTokenWasInvalidated()
    }

    func voIPPushTokenDidRotate(_ token: Data) {
        guard let bundleID = Bundle.main.bundleIdentifier else {
            authoritySink?.nativeDiagnostic("apns_voip_topic_unavailable")
            return
        }
        authoritySink?.nativeVoIPTokenDidRotate(token, topic: "\(bundleID).voip")
    }

    func voIPPushTokenWasInvalidated() {
        authoritySink?.nativeVoIPTokenWasInvalidated()
    }

    func pushDiagnostic(_ code: String) {
        authoritySink?.nativeDiagnostic(code)
    }
}
