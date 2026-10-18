package com.aokie.companion

import android.Manifest
import android.annotation.SuppressLint
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.provider.Settings
import android.view.WindowManager
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.content.ContextCompat

class MainActivity : TauriActivity() {
  private val microphonePermissionResults = mutableMapOf<Int, Int>()
  private val notificationPermissionResults = mutableMapOf<Int, Int>()

  /** What Android offered when the notification request went out, to read its answer by (see [answerIsFinal]). */
  private var notificationRequestedAtMs = 0L
  private var notificationRationaleBefore = false

  /** The page, once Tauri has created the WebView (see [onWebViewCreate]). */
  private var page: WebView? = null

  /** This activity as the Back policy sees it (see [handleBack]). */
  private val backHost = object : BackHost {
    override fun pageCanGoBack(): Boolean = page?.canGoBack() == true

    override fun goBackInPage() {
      page?.goBack()
    }

    override fun moveTaskToBackground() {
      moveTaskToBack(true)
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    // Must precede super.onCreate, which starts the Rust runtime: the native start-up here needs a
    // Java frame with the app's class loader (see AokieNativeInit).
    AokieNativeInit.initialize(this)
    super.onCreate(savedInstanceState)
    // Back before Tauri's own Back callback exists (it is added once the runtime has loaded its app plugin, which
    // then calls onBackPressed below when the page cannot go back): without this, Android's default would finish
    // the activity. See AokieBackPolicy.
    onBackPressedDispatcher.addCallback(
      this,
      object : OnBackPressedCallback(true) {
        override fun handleOnBackPressed() = handleBack(backHost)
      },
    )
    AokieCallNotifications.createChannels(this)
    AokieAudioRoutes.initialize(this)
    AokieOfferStore.current(this, clearExpired = true)
    AokiePushRegistration.ensure(this)
    handleWakeIntent(intent)
  }

  override fun onWebViewCreate(webView: WebView) {
    page = webView
  }

  override fun onDestroy() {
    page = null
    super.onDestroy()
  }

  /**
   * Tauri's Back callback ends here when the page cannot go back (it would otherwise call the platform default,
   * which finishes the activity). Back moves the task to the background instead; see AokieBackPolicy for why
   * finishing the activity is never the answer. Not calling super is the point: the default is the finish.
   */
  @SuppressLint("MissingSuperCall")
  @Suppress("OVERRIDE_DEPRECATION")
  override fun onBackPressed() = handleBack(backHost)

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    setIntent(intent)
    handleWakeIntent(intent)
  }

  private fun handleWakeIntent(intent: Intent?) {
    if (intent?.action == ACTION_ANSWER_OFFER) {
      // The call notification's Answer button opens this activity directly: Android 12 and later refuse an
      // activity start from a service that a notification started, so the claim used to run with no app on
      // screen and no way to bring it up. The service now runs the same claim as a system answer.
      val offerId = intent.getStringExtra(AokieIncomingCallService.EXTRA_OFFER_ID)
      // Consumed once: a recreated activity must not answer again.
      setIntent(Intent(this, MainActivity::class.java).setAction(Intent.ACTION_MAIN))
      runCatching {
        startService(
          Intent(this, AokieIncomingCallService::class.java)
            .setAction(AokieIncomingCallService.ACTION_ANSWER)
            .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, offerId),
        )
      }.onFailure { AokieOfferStore.recordDiagnostic(this, "notification_answer_forward_failed") }
      return
    }
    if (shouldWakeForAuthoritativeTransfer(intent?.action)) {
      // The intent contains no assistance content. Bringing the authenticated
      // app forward is the action; v2 sync supplies current authoritative data.
      AokieOfferStore.recordDiagnostic(this, "assistance_offer_opened_for_authoritative_refresh")
      // Light the display for this exact transfer action only. Never opt into
      // showWhenLocked: the privacy-safe CallStyle can appear on a secure lock
      // screen, while transcript/caller data remains behind device unlock.
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O_MR1) {
        setShowWhenLocked(false)
        setTurnScreenOn(true)
        window.decorView.postDelayed({ setTurnScreenOn(false) }, 2_000L)
      } else {
        window.addFlags(WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON)
        window.decorView.postDelayed(
          { window.clearFlags(WindowManager.LayoutParams.FLAG_TURN_SCREEN_ON) },
          2_000L,
        )
      }
    }
  }

  /*
   * Every method below is called by Rust through JNI. None may throw: an exception left pending across that
   * call aborts the app at the next JNI call on the main thread (Rust also clears one if it slips through).
   * Each failure becomes the method's own failure value. The one deliberate exception is a secure-store read
   * error (see getAokieSecureValue), which Rust clears and reports as an error.
   */

  /**
   * Called only by the native media manager after a consult/talk lease is
   * exact and current. Monitor creation never reaches this method.
   *
   *  1 = granted, 0 = system dialog pending, -1 = denied.
   */
  fun requestAokieMicrophonePermission(requestId: Int): Int = runCatching {
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
      microphonePermissionResults[requestId] = 1
      // A session that began before the grant now needs its microphone service type.
      AokieCommunicationService.refresh(this)
      return@runCatching 1
    }
    if (microphonePermissionResults[requestId] == 0) return@runCatching 0
    microphonePermissionResults[requestId] = 0
    requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), requestId)
    0
  }.getOrElse {
    microphonePermissionResults.remove(requestId)
    -1
  }

  fun pollAokieMicrophonePermission(requestId: Int): Int = runCatching {
    if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED) {
      microphonePermissionResults[requestId] = 1
      return@runCatching 1
    }
    val previous = microphonePermissionResults[requestId]
    if (previous == 1) microphonePermissionResults[requestId] = -1
    microphonePermissionResults[requestId] ?: -1
  }.getOrDefault(-1)

  /** Notification permission is requested separately from microphone access. */
  fun requestAokieNotificationPermission(requestId: Int): Int = runCatching {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    ) {
      notificationPermissionResults[requestId] = 1
      return@runCatching 1
    }
    if (notificationPermissionResults[requestId] == 0) return@runCatching 0
    notificationPermissionResults[requestId] = 0
    notificationRationaleBefore = shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)
    notificationRequestedAtMs = SystemClock.elapsedRealtime()
    requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), requestId)
    0
  }.getOrElse {
    notificationPermissionResults.remove(requestId)
    -1
  }

  fun pollAokieNotificationPermission(requestId: Int): Int = runCatching {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU ||
      ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED
    ) {
      notificationPermissionResults[requestId] = 1
      return@runCatching 1
    }
    val previous = notificationPermissionResults[requestId]
    if (previous == 1) notificationPermissionResults[requestId] = -1
    notificationPermissionResults[requestId] ?: -1
  }.getOrDefault(-1)

  /**
   * Opens this app's notification settings, where the user can allow notifications once Android no longer
   * shows the permission dialog. 1 = opened, -1 = this device has no such screen.
   */
  fun openAokieNotificationSettings(): Int = runCatching {
    val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, packageName)
    } else {
      Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", packageName, null))
    }
    startActivity(intent)
    1
  }.getOrElse { -1 }

  /** Narrow JNI surface. Values never enter WebView storage or callbacks. */
  fun putAokieSecureValue(key: String, value: String): Int =
    if (runCatching { AokieSecureStore.put(this, key, value) }.getOrDefault(false)) 1 else -1

  /**
   * `null` only when nothing is stored under [key]. A value that exists but cannot be read (a Keystore
   * hiccup, a damaged file) throws instead: Rust clears the exception and reports a read error, so the
   * caller keeps its identity, session and profiles rather than replacing what it took to be missing.
   */
  fun getAokieSecureValue(key: String): String? =
    AokieSecureStore.read(this, key).getOrElse {
      throw IllegalStateException("Aokie secure value could not be read")
    }

  fun deleteAokieSecureValue(key: String): Int =
    if (runCatching { AokieSecureStore.delete(this, key) }.getOrDefault(false)) 1 else -1

  fun aokieSecureStoreAvailable(): Int =
    if (runCatching { AokieSecureStore.isAvailable(this) }.getOrDefault(false)) 1 else -1

  /** `null` when the snapshot cannot be taken; Rust reports diagnostics as unavailable. */
  fun aokieRuntimeDiagnostics(): String? = runCatching { AokieRuntimeDiagnostics.snapshot(this) }.getOrNull()

  /** Invalidates both the local provider token and its server-registration marker. */
  fun invalidateAokiePushToken(): Int = runCatching { AokiePushRegistration.invalidate(this) }.getOrDefault(-1)

  /** Core-Telecom actions cross this JNI-only queue and never enter the WebView. */
  fun takeAokieNativeCallAction(): String? =
    runCatching { AokieNativeCallActionStore.takeEncoded(this) }.getOrNull()

  fun completeAokieNativeCallAction(actionId: String, accepted: Int, code: String): Int =
    if (runCatching { AokieNativeCallActionStore.complete(this, actionId, accepted > 0, code) }.getOrDefault(false)) 1 else -1

  /**
   * Presents only a signed offer already revalidated by the native Rust v2
   * client. The encoded value remains in the JNI/Keystore boundary and never
   * enters WebView state. An opaque push can wake the app, but cannot populate
   * the transfer request binding accepted by Answer or Decline.
   */
  fun presentAokieAuthoritativeOffer(encoded: String): Int = runCatching {
    val offer = AokieVoiceOffer.fromAuthoritativeJson(encoded) ?: return@runCatching -1
    val committed = AokieOfferStore.acceptAuthoritative(this, offer) ?: return@runCatching -1
    if (!AokieCallNotifications.notificationsAllowed(this)) return@runCatching -1
    val intent = Intent(this, AokieIncomingCallService::class.java)
      .setAction(AokieIncomingCallService.ACTION_PRESENT)
      .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, committed.offerId)
    runCatching {
      ContextCompat.startForegroundService(this, intent)
      1
    }.getOrElse {
      AokieOfferStore.cancel(this, committed.offerId, "authoritative_offer_foreground_start_failed")
      -1
    }
  }.getOrDefault(-1)

  /** System-owned communication routes; no Bluetooth address crosses JNI. */
  fun beginAokieCommunicationAudio(): Int = runCatching { AokieAudioRoutes.begin(this) }.getOrDefault(-1)

  fun endAokieCommunicationAudio(): Int = runCatching { AokieAudioRoutes.end(this) }.getOrDefault(-1)

  /** `null` when the routes cannot be read; Rust reports the route list as unavailable. */
  fun aokieAudioRoutes(): String? = runCatching { AokieAudioRoutes.snapshot(this) }.getOrNull()

  /**
   * `null` when Android refuses the route (it vanished, the session ended, Android rejected it): the route
   * picker reports the failure. A headset that drops while the user picks it must not crash the call.
   */
  fun selectAokieAudioRoute(routeId: String): String? =
    runCatching { AokieAudioRoutes.select(this, routeId) }
      .onFailure { AokieOfferStore.recordDiagnostic(this, "communication_route_select_failed") }
      .getOrNull()

  fun signalAokieLiveTransition(): Int = runCatching { AokieAudioRoutes.signalLiveTransition(this) }.getOrDefault(-1)

  /**
   * Reconciles a native ringing surface with current v2 authority. "won" is
   * accepted only for the exact pending call epoch. Any other outcome closes
   * only the Companion leg and cannot control the cellular call.
   */
  fun reconcileAokieOffer(callId: String, callEpoch: Long, outcome: String, reason: String): Int = runCatching {
    val current = AokieOfferStore.current(this, clearExpired = true) ?: return@runCatching 0
    if (current.callId != callId || current.callEpoch != callEpoch) return@runCatching 0
    when (outcome) {
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
        val cancelled = AokieOfferStore.cancelByCall(this, callId, callEpoch, reason) ?: return@runCatching 0
        startService(
          Intent(this, AokieIncomingCallService::class.java)
            .setAction(AokieIncomingCallService.ACTION_CANCEL)
            .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, cancelled.offerId),
        )
        1
      }
      else -> -1
    }
  }.getOrElse {
    // Android refused the service start (it can while the app is in the background). A cancel has already
    // removed the record; a refused "won" leaves the offer to its own setup deadline, which returns the lease.
    AokieOfferStore.recordDiagnostic(this, "offer_reconcile_service_start_failed")
    -1
  }

  override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    if (permissions.any { it == Manifest.permission.RECORD_AUDIO }) {
      val granted = grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED
      microphonePermissionResults[requestCode] = if (granted) 1 else -1
      if (granted) AokieCommunicationService.refresh(this)
    }
    if (permissions.any { it == Manifest.permission.POST_NOTIFICATIONS }) {
      val granted = grantResults.isNotEmpty() && grantResults[0] == PackageManager.PERMISSION_GRANTED
      notificationPermissionResults[requestCode] = if (granted) 1 else -1
      // Remember when Android will not show the question again, so the setup screen can send the user to the
      // notification settings instead of a button that cannot show the dialog. A dialog the user merely closed
      // does not count: see answerIsFinal.
      AokieNotificationPrompt.recordAnswer(
        this,
        answerIsFinal(
          granted = granted,
          canAskAgainBefore = notificationRationaleBefore,
          canAskAgainAfter = shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS),
          answeredAfterMs = SystemClock.elapsedRealtime() - notificationRequestedAtMs,
        ),
      )
    }
  }

  companion object {
    const val ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE =
      "com.aokie.companion.action.REFRESH_AUTHORITATIVE_ASSISTANCE"
    /** The call notification's Answer button; carries [AokieIncomingCallService.EXTRA_OFFER_ID]. */
    const val ACTION_ANSWER_OFFER = "com.aokie.companion.action.ANSWER_FROM_NOTIFICATION"
  }
}

internal fun shouldWakeForAuthoritativeTransfer(action: String?): Boolean =
  action == MainActivity.ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE
