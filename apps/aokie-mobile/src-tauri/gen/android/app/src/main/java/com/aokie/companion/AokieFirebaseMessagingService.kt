package com.aokie.companion

import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat
import com.google.firebase.FirebaseApp
import com.google.firebase.messaging.FirebaseMessaging
import com.google.firebase.messaging.FirebaseMessagingService
import com.google.firebase.messaging.RemoteMessage

/**
 * FCM is an optional wake transport. No Firebase project credential is present
 * in source control; a customer/vendor build supplies app/google-services.json.
 * Push data never becomes call authority and never contains SDP, ICE, tokens,
 * captions, or a caller number.
 */
class AokieFirebaseMessagingService : FirebaseMessagingService() {
  override fun onNewToken(token: String) {
    super.onNewToken(token)
    if (token.length !in 16..4_096 || token.any(Char::isISOControl)) {
      AokieOfferStore.recordDiagnostic(this, "fcm_token_invalid")
      return
    }
    if (AokieSecureStore.put(this, AokiePushRegistration.FCM_TOKEN_KEY, token)) {
      AokieSecureStore.delete(this, AokiePushRegistration.FCM_REGISTRATION_KEY)
      // A native Rust watcher consumes this token through the Keystore bridge
      // and enrolls it with the active OAuth session. React never receives it.
      AokieOfferStore.recordDiagnostic(this, "fcm_endpoint_registration_required")
    } else {
      AokieOfferStore.recordDiagnostic(this, "fcm_token_secure_store_failed")
    }
  }

  override fun onMessageReceived(message: RemoteMessage) {
    super.onMessageReceived(message)
    val data = message.data
    when (data["aokieClass"]) {
      "voice_offer" -> receiveVoiceOffer(data)
      "voice_offer_cancel" -> receiveVoiceOfferCancel(data)
      "assistance_offer" -> receiveAssistanceOffer(data)
      "informational" -> receiveInformational(data)
      else -> AokieOfferStore.recordDiagnostic(this, "push_class_rejected")
    }
  }

  private fun receiveVoiceOffer(data: Map<String, String>) {
    val offer = AokieVoiceOffer.fromPush(data)
    if (offer == null || !AokieOfferStore.acceptPush(this, offer)) {
      AokieOfferStore.recordDiagnostic(this, "voice_offer_invalid_or_stale")
      return
    }
    if (!AokieCallNotifications.notificationsAllowed(this)) {
      AokieOfferStore.recordDiagnostic(this, "voice_offer_notification_permission_denied")
      return
    }
    val intent = Intent(this, AokieIncomingCallService::class.java)
      .setAction(AokieIncomingCallService.ACTION_PRESENT)
      .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, offer.offerId)
    runCatching { ContextCompat.startForegroundService(this, intent) }
      .onFailure {
        AokieOfferStore.cancel(this, offer.offerId, "voice_offer_foreground_start_failed")
      }
  }

  private fun receiveVoiceOfferCancel(data: Map<String, String>) {
    val cancellation = parseVoiceOfferCancel(data)
    if (cancellation == null) {
      AokieOfferStore.recordDiagnostic(this, "voice_offer_cancel_invalid")
      return
    }
    val cancelled = AokieOfferStore.cancel(this, cancellation.offerId, cancellation.reason) ?: return
    startService(
      Intent(this, AokieIncomingCallService::class.java)
        .setAction(AokieIncomingCallService.ACTION_CANCEL)
        .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, cancelled.offerId),
    )
  }

  private fun receiveInformational(data: Map<String, String>) {
    val allowed = setOf("aokieClass", "schemaVersion", "eventId", "title", "body", "expiresAt")
    if (data.keys.any { it !in allowed } || data["schemaVersion"] != "1") {
      AokieOfferStore.recordDiagnostic(this, "informational_push_invalid")
      return
    }
    val eventId = data["eventId"]?.takeIf(::safeId) ?: return
    val title = data["title"]?.takeIf { safeText(it, 80) } ?: "Aokie update"
    val body = data["body"]?.takeIf { safeText(it, 240) } ?: "Open Aokie Companion for current call status."
    val now = System.currentTimeMillis() / 1000
    val expiresAt = data["expiresAt"]?.toLongOrNull() ?: return
    if (expiresAt <= now || expiresAt > now + 24 * 60 * 60) return
    AokieCallNotifications.informational(this, eventId, title, body)
  }

  /**
   * An assistance push is an opaque, short wake hint. It deliberately omits
   * the question, answer choices, captions, caller identity, credentials and
   * all media data. Opening it only resumes Companion so authenticated v2
   * state can supply the authoritative request.
   */
  private fun receiveAssistanceOffer(data: Map<String, String>) {
    val allowed = setOf(
      "aokieClass", "schemaVersion", "eventId", "appId", "requestId",
      "callId", "callEpoch", "ownerEpoch", "expiresAt",
    )
    if (data.keys.any { it !in allowed } || data["schemaVersion"] != "1") {
      AokieOfferStore.recordDiagnostic(this, "assistance_offer_push_invalid")
      return
    }
    val eventId = data["eventId"]?.takeIf(::safeId) ?: return
    data["appId"]?.takeIf(::safeId) ?: return
    data["requestId"]?.takeIf(::safeId) ?: return
    data["callId"]?.takeIf(::safeId) ?: return
    val callEpoch = data["callEpoch"]?.toLongOrNull() ?: return
    val ownerEpoch = data["ownerEpoch"]?.toLongOrNull() ?: return
    val maximum = 9_007_199_254_740_991L
    if (callEpoch !in 1..maximum || ownerEpoch !in 0..maximum) return
    val now = System.currentTimeMillis() / 1000
    val expiresAt = data["expiresAt"]?.toLongOrNull() ?: return
    if (expiresAt <= now || expiresAt > now + 5 * 60) return
    AokieCallNotifications.assistanceOffer(this, eventId, expiresAt)
    AokieOfferStore.recordDiagnostic(this, "assistance_offer_authoritative_refresh_required")
  }

  private fun safeId(value: String): Boolean =
    value.length in 1..200 && value.all { it.isLetterOrDigit() || it in "-_.:" }

  private fun safeText(value: String, maximum: Int): Boolean =
    value.length in 1..maximum && value.none(Char::isISOControl)
}

internal data class AokieVoiceOfferCancellation(val offerId: String, val reason: String)

internal fun parseVoiceOfferCancel(data: Map<String, String>): AokieVoiceOfferCancellation? {
  val allowed = setOf("aokieClass", "schemaVersion", "eventId", "offerId", "reason")
  if (data.keys.any { it !in allowed } || data["aokieClass"] != "voice_offer_cancel" ||
    data["schemaVersion"] != "1"
  ) return null
  data["eventId"]?.takeIf {
    it.length in 1..200 && it.all { character -> character.isLetterOrDigit() || character in "-_.:" }
  } ?: return null
  val offerId = data["offerId"]?.takeIf {
    it.length in 1..200 && it.all { character -> character.isLetterOrDigit() || character in "-_.:" }
  } ?: return null
  val reason = data["reason"]
    ?.takeIf { it.length in 1..120 && it.none(Char::isISOControl) }
    ?: "authoritative_cancel"
  return AokieVoiceOfferCancellation(offerId, reason)
}

internal object AokiePushRegistration {
  const val FCM_TOKEN_KEY = "aokie.fcm-registration-token.v1"
  const val FCM_REGISTRATION_KEY = "aokie.fcm-registration-fingerprint.v1"

  fun ensure(context: Context) {
    if (!BuildConfig.AOKIE_FCM_CONFIG_PRESENT) {
      AokieOfferStore.recordDiagnostic(context, "fcm_configuration_required")
      return
    }
    val firebase = runCatching {
      FirebaseApp.getApps(context).firstOrNull() ?: FirebaseApp.initializeApp(context)
    }.getOrNull()
    if (firebase == null) {
      AokieOfferStore.recordDiagnostic(context, "fcm_initialization_failed")
      return
    }
    FirebaseMessaging.getInstance().token
      .addOnSuccessListener { token ->
        if (token.length in 16..4_096 && token.none(Char::isISOControl)) {
          val previous = AokieSecureStore.get(context, FCM_TOKEN_KEY)
          AokieSecureStore.put(context, FCM_TOKEN_KEY, token)
          if (previous != token) AokieSecureStore.delete(context, FCM_REGISTRATION_KEY)
          AokieOfferStore.recordDiagnostic(
            context,
            if (registrationCurrent(context)) "fcm_endpoint_registered" else "fcm_endpoint_registration_required",
          )
        }
      }
      .addOnFailureListener {
        AokieOfferStore.recordDiagnostic(context, "fcm_token_pending")
      }
  }

  fun configured(context: Context): Boolean =
    BuildConfig.AOKIE_FCM_CONFIG_PRESENT && FirebaseApp.getApps(context).isNotEmpty()

  fun tokenPresent(context: Context): Boolean = AokieSecureStore.contains(context, FCM_TOKEN_KEY)

  fun registrationCurrent(context: Context): Boolean {
    val token = AokieSecureStore.get(context, FCM_TOKEN_KEY) ?: return false
    val registered = AokieSecureStore.get(context, FCM_REGISTRATION_KEY) ?: return false
    val digest = java.security.MessageDigest.getInstance("SHA-256")
      .digest(token.toByteArray(Charsets.UTF_8))
      .joinToString(separator = "") { byte -> "%02x".format(byte.toInt() and 0xff) }
    return registered == digest
  }

  fun invalidate(context: Context): Int {
    val tokenDeleted = AokieSecureStore.delete(context, FCM_TOKEN_KEY)
    val markerDeleted = AokieSecureStore.delete(context, FCM_REGISTRATION_KEY)
    if (BuildConfig.AOKIE_FCM_CONFIG_PRESENT) {
      runCatching { FirebaseMessaging.getInstance().deleteToken() }
        .getOrNull()
        ?.addOnFailureListener { AokieOfferStore.recordDiagnostic(context, "fcm_provider_invalidation_failed") }
    }
    AokieOfferStore.recordDiagnostic(context, "fcm_endpoint_invalidated")
    return if (tokenDeleted && markerDeleted) 1 else -1
  }
}
