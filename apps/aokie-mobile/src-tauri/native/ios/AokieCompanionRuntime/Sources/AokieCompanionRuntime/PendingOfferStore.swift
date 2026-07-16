import Foundation

final class AokiePendingOfferStore {
    private static let account = "pending-voice-offer-v1"
    private let store: AokieKeychainStore
    private let lock = NSRecursiveLock()

    init(store: AokieKeychainStore) {
        self.store = store
    }

    func accept(_ offer: AokieVoiceOffer) throws -> AokieVoiceOffer? {
        lock.lock()
        defer { lock.unlock() }
        if let current = try current(clearExpired: true) {
            if current.offerID == offer.offerID {
                return current.hasSamePushIdentity(as: offer) ? current : nil
            }
            guard offer.callEpoch > current.callEpoch else { return nil }
        }
        let validated = try offer.validated(allowExpired: false)
        try save(validated)
        return validated
    }

    func current(clearExpired: Bool = true) throws -> AokieVoiceOffer? {
        lock.lock()
        defer { lock.unlock() }
        guard let data = try store.get(account: Self.account), data.count <= 16 * 1024 else {
            return nil
        }
        let offer: AokieVoiceOffer
        do {
            offer = try JSONDecoder()
                .decode(AokieVoiceOffer.self, from: data)
                .validated(allowExpired: true)
        } catch {
            try? store.delete(account: Self.account)
            throw error
        }
        if clearExpired, offer.isExpired {
            try store.delete(account: Self.account)
            return nil
        }
        return offer
    }

    func offer(callUUID: UUID) throws -> AokieVoiceOffer? {
        let offer = try current(clearExpired: true)
        return offer?.callUUID == callUUID ? offer : nil
    }

    func markAnswerRequested(callUUID: UUID) throws -> AokieVoiceOffer? {
        lock.lock()
        defer { lock.unlock() }
        guard var offer = try offer(callUUID: callUUID) else { return nil }
        if offer.status == .answerRequested { return offer }
        guard offer.status == .ringing else { return nil }
        offer.status = .answerRequested
        try save(offer)
        return offer
    }

    func markWon(callID: String, callEpoch: UInt64) throws -> AokieVoiceOffer? {
        lock.lock()
        defer { lock.unlock() }
        guard var offer = try current(clearExpired: true),
              offer.callID == callID,
              offer.callEpoch == callEpoch,
              offer.status == .answerRequested
        else { return nil }
        offer.status = .won
        try save(offer)
        return offer
    }

    @discardableResult
    func cancel(offerID: String? = nil) throws -> AokieVoiceOffer? {
        lock.lock()
        defer { lock.unlock() }
        guard let offer = try current(clearExpired: false),
              offerID == nil || offer.offerID == offerID
        else { return nil }
        try store.delete(account: Self.account)
        return offer
    }

    private func save(_ offer: AokieVoiceOffer) throws {
        let data = try JSONEncoder().encode(offer)
        guard data.count <= 16 * 1024 else { throw AokieNativeRuntimeError.encoding }
        try store.put(account: Self.account, data: data)
    }
}
