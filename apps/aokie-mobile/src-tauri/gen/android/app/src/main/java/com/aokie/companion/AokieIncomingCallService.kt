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
import java.util.concurrent.atomic.AtomicLong

internal data class AokieCallFence(val offerId: String, val generation: Long)

internal fun aokieCallActionApplies(action: AokieCallFence, active: AokieCallFence?): Boolean =
  action == active

internal fun aokieOfferExpiryApplies(
  action: AokieCallFence,
  active: AokieCallFence?,
  actionDeadlineAt: Long,
  currentOfferId: String?,
  currentStatus: String?,
  currentDeadlineAt: Long?,
  nowSeconds: Long,
): Boolean = action == active && currentOfferId == action.offerId &&
  currentStatus in setOf(AokieVoiceOffer.STATUS_RINGING, AokieVoiceOffer.STATUS_ANSWER_REQUESTED) &&
  currentDeadlineAt == actionDeadlineAt && nowSeconds >= actionDeadlineAt

internal enum class AokieSystemEndRoute { DECLINE_RINGING, HANG_UP_CALLER, RETURN_TO_AOKIE }
internal enum class AokieSystemHangupResultRoute { CALLER_ENDED, RETURN_COMPANION_LEASE }

internal fun aokieSystemEndRoute(status: String?, disconnectCause: Int): AokieSystemEndRoute = when {
  status == AokieVoiceOffer.STATUS_RINGING &&
    disconnectCause in setOf(DisconnectCause.REJECTED, DisconnectCause.LOCAL) ->
    AokieSystemEndRoute.DECLINE_RINGING
  status == AokieVoiceOffer.STATUS_WON && disconnectCause == DisconnectCause.LOCAL ->
    AokieSystemEndRoute.HANG_UP_CALLER
  else -> AokieSystemEndRoute.RETURN_TO_AOKIE
}

internal fun aokieSystemHangupResultRoute(confirmed: Boolean): AokieSystemHangupResultRoute =
  if (confirmed) AokieSystemHangupResultRoute.CALLER_ENDED
  else AokieSystemHangupResultRoute.RETURN_COMPANION_LEASE

/**
 * Owns the Android system-call surface independently of the WebView.
 *
 * Answer means "request the authoritative v2 claim", never "open the mic".
 * Rust marks a matching call epoch won only after the gateway/Desktop lease is
 * active. A local user hang-up on that exact Won call uses the authenticated
 * caller-ending challenge; non-user teardown still returns only the Companion
 * lease.
 */
class AokieIncomingCallService : Service() {
  companion object {
    const val ACTION_PRESENT = "com.aokie.companion.action.PRESENT_OFFER"
    const val ACTION_ANSWER = "com.aokie.companion.action.ANSWER_OFFER"
    const val ACTION_DECLINE = "com.aokie.companion.action.DECLINE_OFFER"
    const val ACTION_HANG_UP = "com.aokie.companion.action.HANG_UP_CALLER"
    const val ACTION_CANCEL = "com.aokie.companion.action.CANCEL_OFFER"
    const val ACTION_WON = "com.aokie.companion.action.OFFER_WON"
    const val EXTRA_OFFER_ID = "aokie_offer_id"
    const val EXTRA_CALL_ID = "aokie_call_id"
    const val EXTRA_CALL_EPOCH = "aokie_call_epoch"
  }

  private sealed interface LocalAction {
    val fence: AokieCallFence
    data class Answer(override val fence: AokieCallFence) : LocalAction
    data class Won(override val fence: AokieCallFence) : LocalAction
    data class AuthoritativeCancel(override val fence: AokieCallFence) : LocalAction
    data class Hangup(override val fence: AokieCallFence) : LocalAction
    data class OfferExpired(
      override val fence: AokieCallFence,
      val deadlineAt: Long,
    ) : LocalAction
    data class End(
      override val fence: AokieCallFence,
      val cause: DisconnectCause,
      val reason: String,
    ) : LocalAction
  }

  private val serviceJob = SupervisorJob()
  private val scope = CoroutineScope(serviceJob + Dispatchers.Default)
  private val actions = Channel<LocalAction>(Channel.BUFFERED)
  private val callStarted = AtomicBoolean(false)
  private val callGeneration = AtomicLong(0)
  private val transferDeclineStarted = AtomicBoolean(false)
  private val hangupStarted = AtomicBoolean(false)
  @Volatile private var activeOffer: AokieVoiceOffer? = null
  @Volatile private var activeFence: AokieCallFence? = null
  @Volatile private var replacementOfferId: String? = null
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
      ACTION_ANSWER -> activeFenceFor(offerId)?.let { actions.trySend(LocalAction.Answer(it)) }
      ACTION_DECLINE -> activeFenceFor(offerId)?.let {
        actions.trySend(LocalAction.End(it, DisconnectCause(DisconnectCause.REJECTED), "declined_locally"))
      }
      ACTION_HANG_UP -> activeFenceFor(offerId)?.let {
        actions.trySend(LocalAction.Hangup(it))
      }
      ACTION_CANCEL -> activeFenceFor(offerId)?.let {
        actions.trySend(LocalAction.AuthoritativeCancel(it))
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
    if (!callStarted.compareAndSet(false, true)) {
      if (activeOffer?.offerId == offer.offerId) {
        activeOffer = offer
        startCallForeground(
          offer,
          if (offer.status == AokieVoiceOffer.STATUS_RINGING) {
            AokieCallNotifications.incoming(this, offer)
          } else {
            AokieCallNotifications.ongoing(this, offer, offer.status == AokieVoiceOffer.STATUS_WON)
          },
        )
      } else {
        replacementOfferId = offer.offerId
        activeFence?.let { actions.trySend(LocalAction.AuthoritativeCancel(it)) }
      }
      return
    }
    val fence = AokieCallFence(offer.offerId, callGeneration.incrementAndGet())
    transferDeclineStarted.set(false)
    hangupStarted.set(false)
    activeOffer = offer
    activeFence = fence
    startCallForeground(offer, AokieCallNotifications.incoming(this, offer))
    scheduleOfferExpiry(offer, fence)
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
      AokieOfferStore.recordDiagnostic(this, "core_telecom_requires_android_8")
      AokieOfferStore.cancel(this, offer.offerId, "core_telecom_unavailable")
      stopSelf()
      return
    }
    scope.launch { registerCall(offer, fence) }
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private suspend fun registerCall(offer: AokieVoiceOffer, fence: AokieCallFence) {
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
        { _ -> onSystemAnswer(offer, fence) },
        { cause -> onSystemDisconnect(offer, fence, cause) },
        { onSystemSetActive(offer, fence) },
        { throw IllegalStateException("Aokie Companion does not support system hold") },
      ) {
        launch { consumeActions(offer, fence) }
      }
    } catch (_: Throwable) {
      AokieOfferStore.cancel(this, offer.offerId, "core_telecom_add_call_failed")
      AokieOfferStore.recordDiagnostic(this, "core_telecom_add_call_failed")
    } finally {
      expiryJob?.cancel()
      releaseAudioFocus()
      if (activeFence == fence) {
        activeOffer = null
        activeFence = null
      }
      stopForeground(STOP_FOREGROUND_REMOVE)
      val replacement = replacementOfferId
      replacementOfferId = null
      if (replacement != null) {
        callStarted.set(false)
        present(replacement)
      } else {
        stopSelf()
      }
    }
  }

  @RequiresApi(Build.VERSION_CODES.O)
  private suspend fun CallControlScope.consumeActions(
    offer: AokieVoiceOffer,
    fence: AokieCallFence,
  ) {
    for (action in actions) {
      if (!aokieCallActionApplies(action.fence, fence) ||
        !aokieCallActionApplies(action.fence, activeFence)
      ) {
        AokieOfferStore.recordDiagnostic(
          this@AokieIncomingCallService,
          "stale_native_call_action_discarded",
        )
        continue
      }
      when (action) {
        is LocalAction.Answer -> {
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
        is LocalAction.Won -> {
          val won = AokieOfferStore.current(this@AokieIncomingCallService, clearExpired = false)
          if (won?.offerId != offer.offerId || won.status != AokieVoiceOffer.STATUS_WON) {
            disconnect(DisconnectCause(DisconnectCause.CANCELED))
            return
          }
          expiryJob?.cancel()
          expiryJob = null
          requestAudioFocus(fence)
          when (setActive()) {
            is CallControlResult.Success -> updateForeground(won, authoritative = true)
            is CallControlResult.Error -> {
              AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "core_telecom_activate_failed")
              disconnect(DisconnectCause(DisconnectCause.ERROR))
              return
            }
          }
        }
        is LocalAction.AuthoritativeCancel -> {
          AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "authoritative_cancel")
          disconnect(DisconnectCause(DisconnectCause.CANCELED))
          return
        }
        is LocalAction.Hangup -> {
          if (hangUpCaller(offer, fence)) {
            AokieOfferStore.cancel(
              this@AokieIncomingCallService,
              offer.offerId,
              "caller_end_confirmed",
            )
            disconnect(DisconnectCause(DisconnectCause.LOCAL))
            return
          }
          AokieOfferStore.current(this@AokieIncomingCallService, clearExpired = false)
            ?.takeIf { it.offerId == offer.offerId && it.status == AokieVoiceOffer.STATUS_WON }
            ?.let { updateForeground(it, authoritative = true) }
        }
        is LocalAction.OfferExpired -> {
          val current = AokieOfferStore.current(
            this@AokieIncomingCallService,
            clearExpired = false,
          )
          if (!aokieOfferExpiryApplies(
              action.fence,
              activeFence,
              action.deadlineAt,
              current?.offerId,
              current?.status,
              current?.expiryDeadlineAt(),
              System.currentTimeMillis() / 1000,
            )
          ) {
            AokieOfferStore.recordDiagnostic(
              this@AokieIncomingCallService,
              "stale_offer_expiry_discarded",
            )
            continue
          }
          val returned = returnCompanionLease(offer, "offer_expired")
          if (!returned) {
            AokieOfferStore.recordDiagnostic(
              this@AokieIncomingCallService,
              "companion_lease_return_unconfirmed",
            )
          }
          AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, "offer_expired")
          disconnect(DisconnectCause(DisconnectCause.MISSED))
          return
        }
        is LocalAction.End -> {
          val current = AokieOfferStore.current(this@AokieIncomingCallService, clearExpired = false)
          if (action.cause.code == DisconnectCause.REJECTED &&
            current?.offerId == offer.offerId && current.status == AokieVoiceOffer.STATUS_RINGING
          ) {
            // End the Android ringing surface first. The private decline then
            // crosses the same authenticated v2 queue as an in-app response;
            // it is never inferred from the push payload.
            disconnect(action.cause)
            val declined = submitTransferDecline(offer)
            if (!declined) {
              AokieOfferStore.recordDiagnostic(this@AokieIncomingCallService, "transfer_decline_unconfirmed")
            }
            AokieOfferStore.cancel(this@AokieIncomingCallService, offer.offerId, action.reason)
            return
          }
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

  private suspend fun onSystemAnswer(offer: AokieVoiceOffer, fence: AokieCallFence) {
    if (!aokieCallActionApplies(fence, activeFence)) {
      throw IllegalStateException("Aokie offer was replaced before answer")
    }
    val answerRequested = AokieOfferStore.markAnswerRequested(this, offer.offerId)
    if (answerRequested == null) {
      throw IllegalStateException("Aokie offer expired before answer")
    }
    scheduleOfferExpiry(answerRequested, fence)
    launchCompanion()
    val pending = awaitAuthoritativeOffer(offer)
      ?: throw IllegalStateException("Aokie offer was not authenticated before answer")
    if (!aokieCallActionApplies(fence, activeFence)) {
      throw IllegalStateException("Aokie offer was replaced during answer")
    }
    updateForeground(pending, authoritative = false)
    val actionId = AokieNativeCallActionStore.enqueue(this, pending, AokieNativeCallAction.ANSWER)
      ?: throw IllegalStateException("Aokie native answer queue is unavailable")
    val result = AokieNativeCallActionStore.awaitResult(this, actionId)
    if (result?.accepted != true) {
      AokieOfferStore.recordDiagnostic(this, result?.code ?: "native_answer_timed_out")
      AokieOfferStore.cancel(this, offer.offerId, "native_answer_not_admitted")
      throw IllegalStateException("Aokie authoritative answer was not admitted")
    }
  }

  private suspend fun onSystemDisconnect(
    offer: AokieVoiceOffer,
    fence: AokieCallFence,
    cause: DisconnectCause,
  ) {
    if (!aokieCallActionApplies(fence, activeFence)) return
    val current = AokieOfferStore.current(this, clearExpired = false)
    when (aokieSystemEndRoute(
      current?.takeIf { it.offerId == offer.offerId }?.status,
      cause.code,
    )) {
      AokieSystemEndRoute.HANG_UP_CALLER -> {
        when (aokieSystemHangupResultRoute(hangUpCaller(offer, fence))) {
          AokieSystemHangupResultRoute.CALLER_ENDED -> {
            AokieOfferStore.cancel(this, offer.offerId, "caller_end_confirmed")
            releaseAudioFocus()
            return
          }
          AokieSystemHangupResultRoute.RETURN_COMPANION_LEASE -> {
            // Telecom is already tearing down this system call surface. If the
            // destructive caller-end operation did not reach the radio, close
            // and return the Companion media lease now; leaving Desktop in
            // HumanActive would strand the caller until a watchdog noticed.
            val returned = returnCompanionLease(offer, "caller_end_unconfirmed")
            if (!returned) {
              AokieOfferStore.recordDiagnostic(this, "caller_end_failback_unconfirmed")
            }
            AokieOfferStore.cancel(this, offer.offerId, "caller_end_unconfirmed")
            releaseAudioFocus()
            if (!returned) {
              throw IllegalStateException("Caller hang-up and Companion failback were not confirmed")
            }
            return
          }
        }
      }
      AokieSystemEndRoute.DECLINE_RINGING -> {
        val reason = "declined_from_system_surface"
        if (!submitTransferDecline(offer)) {
          AokieOfferStore.recordDiagnostic(this, "transfer_decline_unconfirmed")
        }
        AokieOfferStore.cancel(this, offer.offerId, reason)
        releaseAudioFocus()
        return
      }
      AokieSystemEndRoute.RETURN_TO_AOKIE -> Unit
    }
    val reason = if (cause.code == DisconnectCause.REJECTED) "declined_from_system_surface" else "companion_leg_ended"
    if (!returnCompanionLease(offer, reason)) {
      AokieOfferStore.recordDiagnostic(this, "companion_lease_return_unconfirmed")
    }
    AokieOfferStore.cancel(this, offer.offerId, reason)
    releaseAudioFocus()
  }

  private suspend fun hangUpCaller(offer: AokieVoiceOffer, fence: AokieCallFence): Boolean {
    if (!aokieCallActionApplies(fence, activeFence)) return false
    val current = AokieOfferStore.current(this, clearExpired = false)
    if (current?.offerId != offer.offerId || current.status != AokieVoiceOffer.STATUS_WON) {
      return false
    }
    if (!hangupStarted.compareAndSet(false, true)) return false
    val actionId = AokieNativeCallActionStore.enqueue(
      this,
      current,
      AokieNativeCallAction.HANGUP,
    )
    if (actionId == null) {
      hangupStarted.set(false)
      return false
    }
    launchCompanion()
    val result = AokieNativeCallActionStore.awaitHangupResult(this, actionId)
    if (result?.accepted == true) return true
    AokieOfferStore.recordDiagnostic(this, result?.code ?: "caller_end_timed_out")
    hangupStarted.set(false)
    return false
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

  private suspend fun awaitAuthoritativeOffer(offer: AokieVoiceOffer): AokieVoiceOffer? {
    val deadline = android.os.SystemClock.elapsedRealtime() + 5_000L
    while (android.os.SystemClock.elapsedRealtime() < deadline) {
      val current = AokieOfferStore.current(this, clearExpired = true)
      if (current?.offerId != offer.offerId) return null
      if (current.authoritative) return current
      delay(50)
    }
    return null
  }

  private suspend fun submitTransferDecline(offer: AokieVoiceOffer): Boolean {
    if (!transferDeclineStarted.compareAndSet(false, true)) return true
    var current = AokieOfferStore.current(this, clearExpired = true) ?: return true
    if (current.offerId != offer.offerId) return true
    if (!current.authoritative) {
      launchCompanion()
      current = awaitAuthoritativeOffer(offer) ?: return false
    }
    if (current.acceptedTransferRequestId == null) return true
    val actionId = AokieNativeCallActionStore.enqueue(
      this,
      current,
      AokieNativeCallAction.DECLINE,
      "declined",
    ) ?: return false
    launchCompanion()
    return AokieNativeCallActionStore.awaitResult(this, actionId)?.accepted == true
  }

  private suspend fun onSystemSetActive(offer: AokieVoiceOffer, fence: AokieCallFence) {
    if (!aokieCallActionApplies(fence, activeFence)) {
      throw IllegalStateException("Aokie offer was replaced before activation")
    }
    val current = AokieOfferStore.current(this)
    if (current?.offerId != offer.offerId || current.status != AokieVoiceOffer.STATUS_WON) {
      throw IllegalStateException("Aokie lease is not authoritative")
    }
    requestAudioFocus(fence)
  }

  private fun reconcileWon(callId: String?, callEpoch: Long) {
    if (callId == null || callEpoch <= 0) return
    val won = AokieOfferStore.markWon(this, callId, callEpoch) ?: return
    val fence = activeFenceFor(won.offerId) ?: return
    // Retire the original offer timer at the authoritative Won transition.
    // A timer already queued is also status-fenced in consumeActions.
    expiryJob?.cancel()
    expiryJob = null
    actions.trySend(LocalAction.Won(fence))
  }

  private fun activeFenceFor(offerId: String?): AokieCallFence? =
    activeFence?.takeIf { fence ->
      offerId != null && fence.offerId == offerId && activeOffer?.offerId == offerId
    }

  private fun scheduleOfferExpiry(offer: AokieVoiceOffer, fence: AokieCallFence) {
    val deadlineAt = offer.expiryDeadlineAt() ?: return
    expiryJob?.cancel()
    expiryJob = scope.launch {
      val remaining = deadlineAt * 1000 - System.currentTimeMillis()
      if (remaining > 0) delay(remaining)
      actions.send(LocalAction.OfferExpired(fence, deadlineAt))
    }
  }

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
  private fun requestAudioFocus(fence: AokieCallFence) {
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
          actions.trySend(
            LocalAction.End(fence, DisconnectCause(DisconnectCause.ERROR), "audio_focus_lost"),
          )
        }
      }
      .build()
    if (audioManager.requestAudioFocus(request) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
      audioFocusRequest = request
    } else {
      AokieOfferStore.recordDiagnostic(this, "audio_focus_denied")
      actions.trySend(
        LocalAction.End(fence, DisconnectCause(DisconnectCause.ERROR), "audio_focus_denied"),
      )
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
