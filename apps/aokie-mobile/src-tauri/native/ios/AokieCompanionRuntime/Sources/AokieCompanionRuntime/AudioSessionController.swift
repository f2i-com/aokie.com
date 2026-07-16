import AVFoundation
import Foundation

public protocol AokieNativeMediaSink: AnyObject {
    /// Called only after exact winner reconciliation, CallKit activation, and
    /// microphone permission. The native WebRTC layer may arm its track here.
    func nativeMediaMayStart(audioSession: AVAudioSession)
    /// Must synchronously disarm capture before returning, then close the peer.
    func nativeMediaMustStop(reason: String)
}

final class AokieAudioSessionController {
    weak var mediaSink: AokieNativeMediaSink?
    private var callKitSession: AVAudioSession?
    private var authorityGranted = false
    private var transmitting = false
    private var permissionTimer: DispatchSourceTimer?
    private var observers: [NSObjectProtocol] = []

    init() {
        let center = NotificationCenter.default
        observers.append(center.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: nil,
            queue: .main
        ) { [weak self] notification in self?.interrupted(notification) })
        observers.append(center.addObserver(
            forName: AVAudioSession.routeChangeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in self?.validatePermission(reason: "audio_route_changed") })
    }

    deinit {
        permissionTimer?.cancel()
        observers.forEach(NotificationCenter.default.removeObserver)
    }

    func prepareForIncomingCall() throws {
        let session = AVAudioSession.sharedInstance()
        try session.setCategory(
            .playAndRecord,
            mode: .voiceChat,
            options: [.allowBluetooth, .defaultToSpeaker]
        )
    }

    func callKitDidActivate(_ session: AVAudioSession) {
        callKitSession = session
        beginIfAuthorized()
    }

    func callKitDidDeactivate(_ session: AVAudioSession) {
        if callKitSession === session { callKitSession = nil }
        stop(reason: "callkit_audio_deactivated")
    }

    func grantAuthority() {
        authorityGranted = true
        beginIfAuthorized()
    }

    func revokeAuthority(reason: String) {
        authorityGranted = false
        stop(reason: reason)
    }

    private func beginIfAuthorized() {
        guard authorityGranted, !transmitting, let session = callKitSession else { return }
        switch AVAudioSession.sharedInstance().recordPermission {
        case .granted:
            transmitting = true
            startPermissionPolling()
            mediaSink?.nativeMediaMayStart(audioSession: session)
        case .undetermined:
            // This request is intentionally after authoritative winner
            // reconciliation, never at notification receipt or CallKit answer.
            AVAudioSession.sharedInstance().requestRecordPermission { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self else { return }
                    if granted { self.beginIfAuthorized() }
                    else { self.stop(reason: "microphone_permission_denied") }
                }
            }
        case .denied:
            stop(reason: "microphone_permission_denied")
        @unknown default:
            stop(reason: "microphone_permission_unknown")
        }
    }

    private func startPermissionPolling() {
        permissionTimer?.cancel()
        let timer = DispatchSource.makeTimerSource(queue: .main)
        timer.schedule(deadline: .now() + .milliseconds(500), repeating: .milliseconds(500))
        timer.setEventHandler { [weak self] in self?.validatePermission(reason: "permission_poll") }
        permissionTimer = timer
        timer.resume()
    }

    private func validatePermission(reason: String) {
        guard transmitting else { return }
        guard AVAudioSession.sharedInstance().recordPermission == .granted else {
            authorityGranted = false
            stop(reason: "microphone_permission_revoked")
            return
        }
        _ = reason
    }

    private func interrupted(_ notification: Notification) {
        guard
            let raw = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
            AVAudioSession.InterruptionType(rawValue: raw) == .began
        else { return }
        authorityGranted = false
        stop(reason: "audio_session_interrupted")
    }

    private func stop(reason: String) {
        permissionTimer?.cancel()
        permissionTimer = nil
        let shouldNotify = transmitting || authorityGranted
        transmitting = false
        authorityGranted = false
        if shouldNotify {
            mediaSink?.nativeMediaMustStop(reason: reason)
        }
    }
}
