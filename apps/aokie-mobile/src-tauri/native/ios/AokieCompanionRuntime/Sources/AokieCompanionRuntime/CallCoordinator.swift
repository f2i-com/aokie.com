import AVFoundation
import CallKit
import Foundation

protocol AokieCallCoordinatorSink: AnyObject {
    func callCoordinatorDidRequestAnswer(_ offer: AokieVoiceOffer)
    func callCoordinatorDidGrantAuthority(_ offer: AokieVoiceOffer)
    func callCoordinatorDidActivate(_ audioSession: AVAudioSession)
    func callCoordinatorDidDeactivate(_ audioSession: AVAudioSession)
    func callCoordinatorDidEnd(_ offer: AokieVoiceOffer?, reason: String)
    func callCoordinatorDiagnostic(_ code: String)
}

final class AokieCallCoordinator: NSObject, CXProviderDelegate {
    weak var sink: AokieCallCoordinatorSink?
    private let provider: CXProvider
    private let store: AokiePendingOfferStore
    private var expiry: DispatchWorkItem?

    init(store: AokiePendingOfferStore) {
        self.store = store
        let configuration = CXProviderConfiguration(localizedName: "Aokie Companion")
        configuration.supportsVideo = false
        configuration.maximumCallGroups = 1
        configuration.maximumCallsPerCallGroup = 1
        configuration.includesCallsInRecents = false
        configuration.supportedHandleTypes = [.generic]
        self.provider = CXProvider(configuration: configuration)
        super.init()
        provider.setDelegate(self, queue: .main)
    }

    func receive(_ offer: AokieVoiceOffer, completion: @escaping (Error?) -> Void) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { completion(AokieNativeRuntimeError.encoding); return }
            do {
                guard let stored = try self.store.accept(offer) else {
                    completion(AokieNativeRuntimeError.staleOffer)
                    return
                }
                // A retransmitted push is idempotent and must not create a
                // second CallKit surface with a new UUID.
                if stored.callUUID != offer.callUUID {
                    completion(nil)
                    return
                }
                let update = CXCallUpdate()
                update.localizedCallerName = "Aokie caller"
                update.remoteHandle = CXHandle(type: .generic, value: "Aokie call")
                update.hasVideo = false
                update.supportsHolding = false
                update.supportsGrouping = false
                update.supportsUngrouping = false
                update.supportsDTMF = false
                self.provider.reportNewIncomingCall(with: offer.callUUID, update: update) { error in
                    if error == nil {
                        self.scheduleExpiry(offer)
                    } else {
                        _ = try? self.store.cancel(offerID: offer.offerID)
                        self.sink?.callCoordinatorDiagnostic("callkit_report_failed")
                    }
                    completion(error)
                }
            } catch {
                self.sink?.callCoordinatorDiagnostic("pending_offer_store_failed")
                completion(error)
            }
        }
    }

    func cancel(offerID: String, reason: String) {
        DispatchQueue.main.async { [weak self] in
            guard let self,
                  let offer = try? self.store.current(clearExpired: false),
                  offer.offerID == offerID
            else { return }
            self.expiry?.cancel()
            _ = try? self.store.cancel(offerID: offerID)
            self.provider.reportCall(with: offer.callUUID, endedAt: Date(), reason: .remoteEnded)
            self.sink?.callCoordinatorDidEnd(offer, reason: reason)
        }
    }

    @discardableResult
    func reconcileWinner(callID: String, callEpoch: UInt64) -> Bool {
        dispatchPrecondition(condition: .onQueue(.main))
        guard let offer = try? store.markWon(callID: callID, callEpoch: callEpoch) else {
            return false
        }
        expiry?.cancel()
        sink?.callCoordinatorDidGrantAuthority(offer)
        return true
    }

    func providerDidReset(_ provider: CXProvider) {
        expiry?.cancel()
        let offer = try? store.cancel()
        sink?.callCoordinatorDidEnd(offer, reason: "callkit_reset")
    }

    func provider(_ provider: CXProvider, perform action: CXAnswerCallAction) {
        do {
            guard let offer = try store.markAnswerRequested(callUUID: action.callUUID) else {
                action.fail()
                return
            }
            // Answer is only a claim request. No microphone or media action is
            // taken until reconcileWinner proves the exact protocol-v2 winner.
            sink?.callCoordinatorDidRequestAnswer(offer)
            action.fulfill()
        } catch {
            sink?.callCoordinatorDiagnostic("callkit_answer_store_failed")
            action.fail()
        }
    }

    func provider(_ provider: CXProvider, perform action: CXEndCallAction) {
        expiry?.cancel()
        let offer = try? store.offer(callUUID: action.callUUID)
        if let offer { _ = try? store.cancel(offerID: offer.offerID) }
        sink?.callCoordinatorDidEnd(offer, reason: "companion_leg_ended")
        action.fulfill()
    }

    func provider(_ provider: CXProvider, didActivate audioSession: AVAudioSession) {
        sink?.callCoordinatorDidActivate(audioSession)
    }

    func provider(_ provider: CXProvider, didDeactivate audioSession: AVAudioSession) {
        sink?.callCoordinatorDidDeactivate(audioSession)
    }

    private func scheduleExpiry(_ offer: AokieVoiceOffer) {
        expiry?.cancel()
        let work = DispatchWorkItem { [weak self] in
            self?.cancel(offerID: offer.offerID, reason: "offer_expired")
        }
        expiry = work
        DispatchQueue.main.asyncAfter(
            deadline: .now() + max(0, offer.expiresAt.timeIntervalSinceNow),
            execute: work
        )
    }
}
