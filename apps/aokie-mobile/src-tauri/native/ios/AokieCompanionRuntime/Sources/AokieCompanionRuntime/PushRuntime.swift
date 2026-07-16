import Foundation
import PushKit

protocol AokiePushRuntimeSink: AnyObject {
    func standardPushTokenDidRotate(_ token: Data)
    func standardPushTokenWasInvalidated()
    func voIPPushTokenDidRotate(_ token: Data)
    func voIPPushTokenWasInvalidated()
    func pushDiagnostic(_ code: String)
}

final class AokiePushRuntime: NSObject, PKPushRegistryDelegate {
    private static let tokenAccount = "apns-voip-token-v1"
    private static let standardTokenAccount = "apns-standard-token-v1"
    weak var sink: AokiePushRuntimeSink?
    private let store: AokieKeychainStore
    private let calls: AokieCallCoordinator
    private var registry: PKPushRegistry?

    init(store: AokieKeychainStore, calls: AokieCallCoordinator) {
        self.store = store
        self.calls = calls
    }

    func start() {
        dispatchPrecondition(condition: .onQueue(.main))
        guard registry == nil else { return }
        let registry = PKPushRegistry(queue: .main)
        registry.delegate = self
        registry.desiredPushTypes = [.voIP]
        self.registry = registry
    }

    /// Called only from the native AppDelegate callback. The token stays in
    /// ThisDeviceOnly Keychain storage and is passed to the native OAuth
    /// registration sink, never to a renderer event or JavaScript callback.
    func didRegisterStandardPushToken(_ token: Data) {
        guard (16...512).contains(token.count) else {
            sink?.pushDiagnostic("apns_token_invalid")
            return
        }
        do {
            try store.put(account: Self.standardTokenAccount, data: token)
            sink?.standardPushTokenDidRotate(token)
        } catch {
            sink?.pushDiagnostic("apns_token_secure_store_failed")
        }
    }

    func didFailStandardPushRegistration() {
        sink?.pushDiagnostic("apns_registration_failed")
    }

    func invalidateStandardPushToken() {
        try? store.delete(account: Self.standardTokenAccount)
        sink?.standardPushTokenWasInvalidated()
    }

    func invalidateAllTokens() {
        dispatchPrecondition(condition: .onQueue(.main))
        try? store.delete(account: Self.standardTokenAccount)
        try? store.delete(account: Self.tokenAccount)
        registry?.desiredPushTypes = []
        sink?.standardPushTokenWasInvalidated()
        sink?.voIPPushTokenWasInvalidated()
    }

    func pushRegistry(
        _ registry: PKPushRegistry,
        didUpdate pushCredentials: PKPushCredentials,
        for type: PKPushType
    ) {
        guard type == .voIP, (16...512).contains(pushCredentials.token.count) else {
            sink?.pushDiagnostic("apns_voip_token_invalid")
            return
        }
        do {
            try store.put(account: Self.tokenAccount, data: pushCredentials.token)
            // The implementing native bridge enrolls this token with the
            // authenticated server. It must never forward it to React.
            sink?.voIPPushTokenDidRotate(pushCredentials.token)
        } catch {
            sink?.pushDiagnostic("apns_token_secure_store_failed")
        }
    }

    func pushRegistry(_ registry: PKPushRegistry, didInvalidatePushTokenFor type: PKPushType) {
        guard type == .voIP else { return }
        try? store.delete(account: Self.tokenAccount)
        sink?.voIPPushTokenWasInvalidated()
    }

    func pushRegistry(
        _ registry: PKPushRegistry,
        didReceiveIncomingPushWith payload: PKPushPayload,
        for type: PKPushType,
        completion: @escaping () -> Void
    ) {
        guard type == .voIP,
              let messageClass = payload.dictionaryPayload["aokieClass"] as? String
        else {
            sink?.pushDiagnostic("voip_push_rejected")
            completion()
            return
        }
        switch messageClass {
        case "voice_offer":
            do {
                let offer = try AokieVoiceOffer.parse(push: payload.dictionaryPayload)
                calls.receive(offer) { [weak self] error in
                    if error != nil { self?.sink?.pushDiagnostic("voice_offer_rejected") }
                    completion()
                }
            } catch {
                sink?.pushDiagnostic("voice_offer_invalid_or_stale")
                completion()
            }
        case "voice_offer_cancel":
            do {
                let cancellation = try AokieVoiceOfferCancellation.parse(push: payload.dictionaryPayload)
                calls.cancel(offerID: cancellation.offerID, reason: cancellation.reason)
            } catch {
                sink?.pushDiagnostic("voice_offer_cancel_invalid")
            }
            completion()
        default:
            // VoIP PushKit is reserved for real incoming-call lifecycle only.
            sink?.pushDiagnostic("voip_push_class_rejected")
            completion()
        }
    }
}
