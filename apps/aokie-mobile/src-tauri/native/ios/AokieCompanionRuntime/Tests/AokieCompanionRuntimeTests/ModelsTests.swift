import XCTest
@testable import AokieCompanionRuntime

final class ModelsTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_784_167_200)

    func testVoiceOfferRequiresExactShortLivedPayload() throws {
        let payload: [AnyHashable: Any] = [
            "aokieClass": "voice_offer",
            "schemaVersion": "1",
            "eventId": "event_1",
            "offerId": "offer_1",
            "appId": "app_1",
            "callId": "call_1",
            "callEpoch": "7",
            "ownerEpoch": "4",
            "expiresAt": String(Int(now.timeIntervalSince1970) + 120),
        ]
        let offer = try AokieVoiceOffer.parse(push: payload, now: now)
        XCTAssertEqual(offer.callID, "call_1")
        XCTAssertEqual(offer.callEpoch, 7)
        XCTAssertEqual(offer.status, .ringing)
    }

    func testVoiceOfferRejectsUnknownKeysAndLongLifetime() {
        var unknown: [AnyHashable: Any] = [
            "aokieClass": "voice_offer", "schemaVersion": "1", "eventId": "event_1",
            "offerId": "offer_1", "appId": "app_1", "callId": "call_1",
            "callEpoch": "7", "ownerEpoch": "4",
            "expiresAt": String(Int(now.timeIntervalSince1970) + 120),
            "callerNumber": "+61000000000",
        ]
        XCTAssertThrowsError(try AokieVoiceOffer.parse(push: unknown, now: now))
        unknown.removeValue(forKey: "callerNumber")
        unknown["expiresAt"] = String(Int(now.timeIntervalSince1970) + 301)
        XCTAssertThrowsError(try AokieVoiceOffer.parse(push: unknown, now: now))
    }

    func testCredentialEnvelopeRequiresTLSAndBoundedSecret() {
        XCTAssertThrowsError(try AokieManagedCredentialEnvelope(
            discoveryURL: "http://formlogic.example/.well-known/aokie-companion",
            deploymentID: "deployment_1",
            appID: "app_1",
            deviceID: "device_1",
            refreshToken: String(repeating: "r", count: 64)
        ))
        XCTAssertNoThrow(try AokieManagedCredentialEnvelope(
            discoveryURL: "https://formlogic.example/.well-known/aokie-companion",
            deploymentID: "deployment_1",
            appID: "app_1",
            deviceID: "device_1",
            refreshToken: String(repeating: "r", count: 64)
        ))
    }
}
