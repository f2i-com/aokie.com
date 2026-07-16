package com.aokie.companion

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  private val microphonePermissionResults = mutableMapOf<Int, Int>()
  private val notificationPermissionResults = mutableMapOf<Int, Int>()

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    AokieCallNotifications.createChannels(this)
    AokieAudioRoutes.initialize(this)
    AokieOfferStore.current(this, clearExpired = true)
    AokiePushRegistration.ensure(this)
    handleWakeIntent(intent)
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    handleWakeIntent(intent)
  }

  private fun handleWakeIntent(intent: Intent?) {
    if (intent?.action == ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE) {
      // The intent contains no assistance content. Bringing the authenticated
      // app forward is the action; v2 sync supplies current authoritative data.
      AokieOfferStore.recordDiagnostic(this, "assistance_offer_opened_for_authoritative_refresh")
    }
  }

  /**
   * Called only by the native media manager after a consult/talk lease is
   * exact and current. Monitor creation never reaches this method.
   *
   *  1 = granted, 0 = system dialog pending, -1 = denied.
   */
  fun requestAokieMicrophonePermission(requestId: Int): Int {
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
      microphonePermissionResults[requestId] = 1
      return 1
    }
    if (microphonePermissionResults[requestId] == 0) return 0
    microphonePermissionResults[requestId] = 0
    requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), requestId)
    return 0
  }

  fun pollAokieMicrophonePermission(requestId: Int): Int {
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
      microphonePermissionResults[requestId] = 1
      return 1
    }
    val previous = microphonePermissionResults[requestId]
    if (previous == 1) microphonePermissionResults[requestId] = -1
    return microphonePermissionResults[requestId] ?: -1
  }

  /** Notification permission is requested separately from microphone access. */
  fun requestAokieNotificationPermission(requestId: Int): Int {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    ) {
      notificationPermissionResults[requestId] = 1
      return 1
    }
    if (notificationPermissionResults[requestId] == 0) return 0
    notificationPermissionResults[requestId] = 0
    requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), requestId)
    return 0
  }

  fun pollAokieNotificationPermission(requestId: Int): Int {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    ) {
      notificationPermissionResults[requestId] = 1
      return 1
    }
    val previous = notificationPermissionResults[requestId]
    if (previous == 1) notificationPermissionResults[requestId] = -1
    return notificationPermissionResults[requestId] ?: -1
  }

  /** Narrow JNI surface. Values never enter WebView storage or callbacks. */
  fun putAokieSecureValue(key: String, value: String): Int =
    if (AokieSecureStore.put(this, key, value)) 1 else -1

  fun getAokieSecureValue(key: String): String? = AokieSecureStore.get(this, key)

  fun deleteAokieSecureValue(key: String): Int =
    if (AokieSecureStore.delete(this, key)) 1 else -1

  fun aokieSecureStoreAvailable(): Int = if (AokieSecureStore.isAvailable(this)) 1 else -1

  fun aokieRuntimeDiagnostics(): String = AokieRuntimeDiagnostics.snapshot(this)

  /** Invalidates both the local provider token and its server-registration marker. */
  fun invalidateAokiePushToken(): Int = AokiePushRegistration.invalidate(this)

  /** Core-Telecom actions cross this JNI-only queue and never enter the WebView. */
  fun takeAokieNativeCallAction(): String? = AokieNativeCallActionStore.takeEncoded(this)

  fun completeAokieNativeCallAction(actionId: String, accepted: Int, code: String): Int =
    if (AokieNativeCallActionStore.complete(this, actionId, accepted > 0, code)) 1 else -1

  /** System-owned communication routes; no Bluetooth address crosses JNI. */
  fun beginAokieCommunicationAudio(): Int = AokieAudioRoutes.begin(this)

  fun endAokieCommunicationAudio(): Int = AokieAudioRoutes.end(this)

  fun aokieAudioRoutes(): String = AokieAudioRoutes.snapshot(this)

  fun selectAokieAudioRoute(routeId: String): String = AokieAudioRoutes.select(this, routeId)

  fun signalAokieLiveTransition(): Int = AokieAudioRoutes.signalLiveTransition(this)

  /**
   * Reconciles a native ringing surface with current v2 authority. "won" is
   * accepted only for the exact pending call epoch. Any other outcome closes
   * only the Companion leg and cannot control the cellular call.
   */
  fun reconcileAokieOffer(callId: String, callEpoch: Long, outcome: String, reason: String): Int {
    val current = AokieOfferStore.current(this, clearExpired = true) ?: return 0
    if (current.callId != callId || current.callEpoch != callEpoch) return 0
    return when (outcome) {
      "won" -> {
        startService(
          Intent(this, AokieIncomingCallService::class.java)
            .setAction(AokieIncomingCallService.ACTION_WON)
            .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, current.offerId)
            .putExtra(AokieIncomingCallService.EXTRA_CALL_ID, callId)
            .putExtra(AokieIncomingCallService.EXTRA_CALL_EPOCH, callEpoch),
        )
        1
      }
      "cancel" -> {
        val cancelled = AokieOfferStore.cancelByCall(this, callId, callEpoch, reason) ?: return 0
        startService(
          Intent(this, AokieIncomingCallService::class.java)
            .setAction(AokieIncomingCallService.ACTION_CANCEL)
            .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, cancelled.offerId),
        )
        1
      }
      else -> -1
    }
  }

  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (permissions.any { it == Manifest.permission.RECORD_AUDIO }) {
      microphonePermissionResults[requestCode] =
        if (grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED) 1 else -1
    }
    if (permissions.any { it == Manifest.permission.POST_NOTIFICATIONS }) {
      notificationPermissionResults[requestCode] =
        if (grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED) 1 else -1
    }
  }

  companion object {
    const val ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE =
      "com.aokie.companion.action.REFRESH_AUTHORITATIVE_ASSISTANCE"
  }
}
