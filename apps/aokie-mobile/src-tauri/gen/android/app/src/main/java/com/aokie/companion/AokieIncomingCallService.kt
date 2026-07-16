package com.aokie.companion

import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFocusRequest
import android.media.AudioManager
import android.net.Uri
import android.os.Build
import android.os.IBinder
import android.telecom.DisconnectCause
import androidx.annotation.RequiresApi
import androidx.core.telecom.CallAttributesCompat
import androidx.core.telecom.CallControlResult
import androidx.core.telecom.CallControlScope
import androidx.core.telecom.CallsManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Owns the Android system-call surface independently of the WebView.
 *
 * Answer means "request the authoritative v2 claim", never "open the mic".
 * Rust marks a matching call epoch won only after the gateway/Desktop lease is
 * active. Decline/end tears down only the Companion leg and cannot hang up the
 * cellular caller.
 */
class AokieIncomingCallService : Service() {
  companion object {
    const val ACTION_PRESENT = "com.aokie.companion.action.PRESENT_OFFER"
    const val ACTION_ANSWER = "com.aokie.companion.action.ANSWER_OFFER"
    const val ACTION_DECLINE = "com.aokie.companion.action.DECLINE_OFFER"
    const val ACTION_CANCEL = "com.aokie.companion.action.CANCEL_OFFER"
    const val ACTION_WON = "com.aokie.companion.action.OFFER_WON"
    const val EXTRA_OFFER_ID = "aokie_offer_id"
    const val EXTRA_CALL_ID = "aokie_call_id"
    const val EXTRA_CALL_EPOCH = "aokie_call_epoch"
  }

  private sealed interface LocalAction {
    data object Answer : LocalAction
    data object Won : LocalAction
    data object AuthoritativeCancel : LocalAction
    data class End(val cause: DisconnectCause, val reason: String) : LocalAction
  }

  private val serviceJob = SupervisorJob()
  private val scope = CoroutineScope(serviceJob + Dispatchers.Default)
  private val actions = Channel<LocalAction>(Channel.BUFFERED)
  private val callStarted = AtomicBoolean(false)
  @Volatile private var activeOffer: AokieVoiceOffer? = null
  private var expiryJob: Job? = null
  private var audioFocusRequest: AudioFocusRequest? = null

  override fun onCreate() {
    super.onCreate()
    AokieCallNotifications.createChannels(this)
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val offerId = intent?.getStringExtra(EXTRA_OFFER_ID)
    when (intent?.action) {
      ACTION_PRESENT -> present(offerId)
      ACTION_ANSWER -> if (matchesActive(offerId)) actions.trySend(LocalAction.Answer)
      ACTION_DECLINE -> if (matchesActive(offerId)) {
        actions.trySend(LocalAction.End(DisconnectCause(DisconnectCause.REJECTED), "declined_locally"))
      }
      ACTION_CANCEL -> if (matchesActive(offerId)) {
        actions.trySend(LocalAction.AuthoritativeCancel)
      }
      ACTION_WON -> reconcileWon(
        intent.getStringExtra(EXTRA_CALL_ID),
        intent.getLongExtra(EXTRA_CALL_EPOCH, -1),
      )
      else -> if (intent != null) AokieOfferStore.recordDiagnostic(this, "call_service_action_rejected")
    }
    return START_NOT_STICKY
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onDestroy() {
    expiryJob?.cancel()
    releaseAudioFocus()
    actions.close()
    serviceJob.cancel()
    super.onDestroy()
  }

  private fun present(expectedOfferId: String?) {
    val offer = AokieOfferStore.current(this, clearExpired = true)
    if (offer == null || offer.offerId != expectedOfferId || !AokieCallNotifications.notificationsAllowed(this)) {
      AokieOfferStore.recordDiagnostic(this, "call_offer_not_presentable")
      stopSelf()
      return
    }
    if (!callStarted.compareAndSet(false, true)) return
    activeOffer = offer
    startCallForeground(offer, AokieCallNotifications.incoming(this, offer))
    expiryJob = scope.launch {
      val remaining = offer.expiresAt * 1000 - System.currentTimeMillis()
      if (remaining > 0) delay(remaining)
      actions.send(LocalAction.End(DisconnectCause(DisconnectCause.MISSED), "offer_expired"))
    }
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
      AokieOfferStore.recordDiagnostic(this, "core_telecom_requires_android_8")
      AokieOfferStore.cancel(this, offer.offerId, "core_telecom_unavailable")
      stopSelf()
      return
    }
    scope.launch { registerCall(offer) }
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private suspend fun registerCall(offer: AokieVoiceOffer) {
    val callsManager = CallsManager(this).apply {
      registerAppWithTelecom(CallsManager.CAPABILITY_BASELINE)
    }
    val attributes = CallAttributesCompat(
      displayName = "Aokie caller",
      address = Uri.parse("aokie:offer/${Uri.encode(offer.offerId)}"),
      direction = CallAttributesCompat.DIRECTION_INCOMING,
      callType = CallAttributesCompat.CALL_TYPE_AUDIO_CALL,
      callCapabilities = 0,
    )
    try {
      callsManager.addCall(
        attributes,
        { _ -> onSystemAnswer(offer) },
        { cause -> onSystemDisconnect(offer, cause) },
        { onSystemSetActive(offer) },
        { throw IllegalStateException("Aokie Companion does not support system hold") },
      ) {
        launch { consumeActions(offer) }
      }
    } catch (_: Throwable) {
      AokieOfferStore.cancel(this, offer.offerId, "core_telecom_add_call_failed")
      AokieOfferStore.recordDiagnostic(this, "core_telecom_add_call_failed")
    } finally {
      expiryJob?.cancel()
      releaseAudioFocus()
      activeOffer = null
      stopForeground(STOP_FOREGROUND_REMOVE)
      stopSelf()
    }
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private suspend fun CallControlScope.consumeActions(offer: AokieVoiceOffer) {
    for (action in actions) {
      when (action) {
        LocalAction.Answer -> {
          if (AokieOfferStore.current(this@AokieIncomingCallService)?.offerId != offer.offerId) {
            disconnect(DisconnectCause(DisconnectCause.CANCELED))
            return
          }
          when (answer(CallAttributesCompat.CALL_TYPE_AUDIO_CALL)) {
            is CallControlResult.Success -> Unit
            is CallControlResult.Error -> {
              AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "core_telecom_answer_failed")
              disconnect(DisconnectCause(DisconnectCause.ERROR))
              return
            }
          }
        }
        LocalAction.Won -> {
          val won = AokieOfferStore.current(this@AokieIncomingCallService)
          if (won?.offerId != offer.offerId || won.status != AokieVoiceOffer.STATUS_WON) {
            disconnect(DisconnectCause(DisconnectCause.CANCELED))
            return
          }
          requestAudioFocus()
          when (setActive()) {
            is CallControlResult.Success -> updateForeground(won, authoritative = true)
            is CallControlResult.Error -> {
              AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "core_telecom_activate_failed")
              disconnect(DisconnectCause(DisconnectCause.ERROR))
              return
            }
          }
        }
        LocalAction.AuthoritativeCancel -> {
          AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "authoritative_cancel")
          disconnect(DisconnectCause(DisconnectCause.CANCELED))
          return
        }
        is LocalAction.End -> {
          val returned = returnCompanionLease(offer, action.reason)
          if (!returned) {
            AokieOfferStore.recordDiagnostic(this@AokieIncomingCallService, "companion_lease_return_unconfirmed")
          }
          AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, action.reason)
          disconnect(action.cause)
          return
        }
      }
    }
  }

  private suspend fun onSystemAnswer(offer: AokieVoiceOffer) {
    if (!AokieOfferStore.markAnswerRequested(this, offer.offerId)) {
      throw IllegalStateException("Aokie offer expired before answer")
    }
    val pending = AokieOfferStore.current(this) ?: throw IllegalStateException("Aokie offer is gone")
    updateForeground(pending, authoritative = false)
    val actionId = AokieNativeCallActionStore.enqueue(this, pending, AokieNativeCallAction.ANSWER)
      ?: throw IllegalStateException("Aokie native answer queue is unavailable")
    launchCompanion()
    val result = AokieNativeCallActionStore.awaitResult(this, actionId)
    if (result?.accepted != true) {
      AokieOfferStore.recordDiagnostic(this, result?.code ?: "native_answer_timed_out")
      AokieOfferStore.cancel(this, offer.offerId, "native_answer_not_admitted")
      throw IllegalStateException("Aokie authoritative answer was not admitted")
    }
  }

  private suspend fun onSystemDisconnect(offer: AokieVoiceOffer, cause: DisconnectCause) {
    val reason = if (cause.code == DisconnectCause.REJECTED) "declined_from_system_surface" else "companion_leg_ended"
    if (!returnCompanionLease(offer, reason)) {
      AokieOfferStore.recordDiagnostic(this, "companion_lease_return_unconfirmed")
    }
    AokieOfferStore.cancel(this, offer.offerId, reason)
    releaseAudioFocus()
  }

  /**
   * Returns only the Companion media lease. This path never emits the
   * separately confirmed End-caller operation.
   */
  private suspend fun returnCompanionLease(offer: AokieVoiceOffer, reason: String): Boolean {
    val current = AokieOfferStore.current(this, clearExpired = false) ?: return true
    if (current.offerId != offer.offerId) return true
    // A never-answered ringing offer cannot own a lease.
    if (current.status == AokieVoiceOffer.STATUS_RINGING) return true
    val actionId = AokieNativeCallActionStore.enqueue(this, current, AokieNativeCallAction.END)
      ?: return false
    launchCompanion()
    val result = AokieNativeCallActionStore.awaitResult(this, actionId)
    if (result?.accepted != true) {
      AokieOfferStore.recordDiagnostic(this, result?.code ?: "lease_return_timed_out")
      return false
    }
    return true
  }

  private suspend fun onSystemSetActive(offer: AokieVoiceOffer) {
    val current = AokieOfferStore.current(this)
    if (current?.offerId != offer.offerId || current.status != AokieVoiceOffer.STATUS_WON) {
      throw IllegalStateException("Aokie lease is not authoritative")
    }
    requestAudioFocus()
  }

  private fun reconcileWon(callId: String?, callEpoch: Long) {
    if (callId == null || callEpoch <= 0) return
    val won = AokieOfferStore.markWon(this, callId, callEpoch) ?: return
    if (activeOffer?.offerId == won.offerId) actions.trySend(LocalAction.Won)
  }

  private fun matchesActive(offerId: String?): Boolean =
    offerId != null && activeOffer?.offerId == offerId

  private fun launchCompanion() {
    runCatching {
      startActivity(
        Intent(this, MainActivity::class.java)
          .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      )
    }.onFailure { AokieOfferStore.recordDiagnostic(this, "call_activity_launch_blocked") }
  }

  private fun startCallForeground(offer: AokieVoiceOffer, notification: android.app.Notification) {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      startForeground(
        AokieCallNotifications.callNotificationId(offer.offerId),
        notification,
        ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL,
      )
    } else {
      startForeground(AokieCallNotifications.callNotificationId(offer.offerId), notification)
    }
  }

  private fun updateForeground(offer: AokieVoiceOffer, authoritative: Boolean) {
    startCallForeground(offer, AokieCallNotifications.ongoing(this, offer, authoritative))
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private fun requestAudioFocus() {
    if (audioFocusRequest != null) return
    val audioManager = getSystemService(AudioManager::class.java)
    val request = AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN_TRANSIENT_EXCLUSIVE)
      .setAudioAttributes(
        AudioAttributes.Builder()
          .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
          .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
          .build(),
      )
      .setAcceptsDelayedFocusGain(false)
      .setOnAudioFocusChangeListener { change ->
        if (change == AudioManager.AUDIOFOCUS_LOSS) {
          AokieOfferStore.recordDiagnostic(this, "audio_focus_lost")
          actions.trySend(LocalAction.End(DisconnectCause(DisconnectCause.ERROR), "audio_focus_lost"))
        }
      }
      .build()
    if (audioManager.requestAudioFocus(request) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
      audioFocusRequest = request
    } else {
      AokieOfferStore.recordDiagnostic(this, "audio_focus_denied")
      actions.trySend(LocalAction.End(DisconnectCause(DisconnectCause.ERROR), "audio_focus_denied"))
    }
  }

  private fun releaseAudioFocus() {
    val request = audioFocusRequest ?: return
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      getSystemService(AudioManager::class.java).abandonAudioFocusRequest(request)
    }
    audioFocusRequest = null
  }
}
