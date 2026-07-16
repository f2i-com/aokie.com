import Foundation
import Security

public enum AokieNativeRuntimeError: Error, Equatable {
    case invalidCredentialEnvelope
    case invalidPushPayload
    case staleOffer
    case secureStore(OSStatus)
    case encoding
}

/// Native-only OAuth restart material. A Tauri plugin may pass this between
/// Rust and Swift, but it must never expose it as an invoke command or event.
public struct AokieManagedCredentialEnvelope: Codable, Equatable, Sendable {
    public let schemaVersion: UInt8
    public let discoveryURL: String
    public let deploymentID: String
    public let appID: String
    public let deviceID: String
    public let refreshToken: String

    public init(
        discoveryURL: String,
        deploymentID: String,
        appID: String,
        deviceID: String,
        refreshToken: String
    ) throws {
        guard
            let url = URL(string: discoveryURL),
            url.scheme == "https",
            url.host != nil,
            Self.safeID(deploymentID),
            Self.safeID(appID),
            Self.safeID(deviceID),
            refreshToken.utf8.count >= 32,
            refreshToken.utf8.count <= 16_384,
            !refreshToken.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else {
            throw AokieNativeRuntimeError.invalidCredentialEnvelope
        }
        self.schemaVersion = 1
        self.discoveryURL = discoveryURL
        self.deploymentID = deploymentID
        self.appID = appID
        self.deviceID = deviceID
        self.refreshToken = refreshToken
    }

    public func validated() throws -> Self {
        guard schemaVersion == 1 else {
            throw AokieNativeRuntimeError.invalidCredentialEnvelope
        }
        return try Self(
            discoveryURL: discoveryURL,
            deploymentID: deploymentID,
            appID: appID,
            deviceID: deviceID,
            refreshToken: refreshToken
        )
    }

    private static func safeID(_ value: String) -> Bool {
        guard !value.isEmpty, value.utf8.count <= 200 else { return false }
        return value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) ||
                (97...122).contains($0) || [45, 46, 58, 95].contains($0)
        }
    }
}

public struct AokieVoiceOffer: Codable, Equatable, Sendable {
    public enum Status: String, Codable, Sendable {
        case ringing
        case answerRequested = "answer_requested"
        case won
    }

    public let schemaVersion: UInt8
    public let eventID: String
    public let offerID: String
    public let appID: String
    public let callID: String
    public let callEpoch: UInt64
    public let ownerEpoch: UInt64
    public let expiresAt: Date
    public let callUUID: UUID
    public var status: Status

    public static func parse(
        push payload: [AnyHashable: Any],
        now: Date = Date()
    ) throws -> Self {
        let allowed = Set([
            "aokieClass", "schemaVersion", "eventId", "offerId", "appId",
            "callId", "callEpoch", "ownerEpoch", "expiresAt",
        ])
        let keys = Set(payload.keys.compactMap { $0 as? String })
        guard
            keys.count == payload.keys.count,
            keys == allowed,
            payload["aokieClass"] as? String == "voice_offer",
            payload["schemaVersion"] as? String == "1",
            let eventID = payload["eventId"] as? String,
            let offerID = payload["offerId"] as? String,
            let appID = payload["appId"] as? String,
            let callID = payload["callId"] as? String,
            let callEpochRaw = payload["callEpoch"] as? String,
            let ownerEpochRaw = payload["ownerEpoch"] as? String,
            let expiresAtRaw = payload["expiresAt"] as? String,
            safeID(eventID), safeID(offerID), safeID(appID), safeID(callID),
            let callEpoch = UInt64(callEpochRaw), callEpoch > 0,
            callEpoch <= 9_007_199_254_740_991,
            let ownerEpoch = UInt64(ownerEpochRaw),
            ownerEpoch <= 9_007_199_254_740_991,
            let expiresAtSeconds = TimeInterval(expiresAtRaw)
        else {
            throw AokieNativeRuntimeError.invalidPushPayload
        }
        let expiresAt = Date(timeIntervalSince1970: expiresAtSeconds)
        guard expiresAt > now, expiresAt.timeIntervalSince(now) <= 5 * 60 else {
            throw AokieNativeRuntimeError.staleOffer
        }
        return Self(
            schemaVersion: 1,
            eventID: eventID,
            offerID: offerID,
            appID: appID,
            callID: callID,
            callEpoch: callEpoch,
            ownerEpoch: ownerEpoch,
            expiresAt: expiresAt,
            callUUID: UUID(),
            status: .ringing
        )
    }

    public var isExpired: Bool { expiresAt <= Date() }

    func validated(now: Date = Date(), allowExpired: Bool) throws -> Self {
        guard
            schemaVersion == 1,
            Self.safeID(eventID), Self.safeID(offerID), Self.safeID(appID), Self.safeID(callID),
            callEpoch > 0, callEpoch <= 9_007_199_254_740_991,
            ownerEpoch <= 9_007_199_254_740_991,
            (allowExpired || expiresAt > now),
            expiresAt.timeIntervalSince(now) <= 5 * 60
        else {
            throw AokieNativeRuntimeError.invalidPushPayload
        }
        return self
    }

    func hasSamePushIdentity(as other: Self) -> Bool {
        eventID == other.eventID && offerID == other.offerID && appID == other.appID &&
            callID == other.callID && callEpoch == other.callEpoch &&
            ownerEpoch == other.ownerEpoch && expiresAt == other.expiresAt
    }

    private static func safeID(_ value: String) -> Bool {
        guard !value.isEmpty, value.utf8.count <= 200 else { return false }
        return value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) ||
                (97...122).contains($0) || [45, 46, 58, 95].contains($0)
        }
    }
}

struct AokieVoiceOfferCancellation {
    let offerID: String
    let reason: String

    static func parse(push payload: [AnyHashable: Any]) throws -> Self {
        let allowed = Set(["aokieClass", "schemaVersion", "eventId", "offerId", "reason"])
        let keys = Set(payload.keys.compactMap { $0 as? String })
        guard
            keys.count == payload.keys.count,
            keys == allowed,
            payload["aokieClass"] as? String == "voice_offer_cancel",
            payload["schemaVersion"] as? String == "1",
            let eventID = payload["eventId"] as? String,
            let offerID = payload["offerId"] as? String,
            let reason = payload["reason"] as? String,
            safeID(eventID), safeID(offerID),
            !reason.isEmpty, reason.utf8.count <= 120,
            !reason.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else {
            throw AokieNativeRuntimeError.invalidPushPayload
        }
        return Self(offerID: offerID, reason: reason)
    }

    private static func safeID(_ value: String) -> Bool {
        guard !value.isEmpty, value.utf8.count <= 200 else { return false }
        return value.utf8.allSatisfy {
            (48...57).contains($0) || (65...90).contains($0) ||
                (97...122).contains($0) || [45, 46, 58, 95].contains($0)
        }
    }
}
