package com.aokie.companion

import android.content.Context
import org.json.JSONObject

internal data class AokieVoiceOffer(
  val eventId: String,
  val offerId: String,
  val appId: String,
  val callId: String,
  val callEpoch: Long,
  val ownerEpoch: Long,
  val expiresAt: Long,
  val status: String = STATUS_RINGING,
) {
  fun isExpired(nowSeconds: Long = System.currentTimeMillis() / 1000): Boolean = expiresAt <= nowSeconds

  fun toJson(): String = JSONObject()
    .put("schemaVersion", 1)
    .put("eventId", eventId)
    .put("offerId", offerId)
    .put("appId", appId)
    .put("callId", callId)
    .put("callEpoch", callEpoch)
    .put("ownerEpoch", ownerEpoch)
    .put("expiresAt", expiresAt)
    .put("status", status)
    .toString()

  fun withStatus(next: String): AokieVoiceOffer = copy(status = next)

  companion object {
    const val STATUS_RINGING = "ringing"
    const val STATUS_ANSWER_REQUESTED = "answer_requested"
    const val STATUS_WON = "won"
    private val statuses = setOf(STATUS_RINGING, STATUS_ANSWER_REQUESTED, STATUS_WON)

    fun fromJson(encoded: String): AokieVoiceOffer? = runCatching {
      val json = JSONObject(encoded)
      require(json.length() == 9 && json.getInt("schemaVersion") == 1)
      AokieVoiceOffer(
        eventId = checkedId(json.getString("eventId")),
        offerId = checkedId(json.getString("offerId")),
        appId = checkedId(json.getString("appId")),
        callId = checkedId(json.getString("callId")),
        callEpoch = checkedEpoch(json.getLong("callEpoch"), allowZero = false),
        ownerEpoch = checkedEpoch(json.getLong("ownerEpoch"), allowZero = true),
        expiresAt = json.getLong("expiresAt"),
        status = json.getString("status").also { require(it in statuses) },
      ).also { require(it.expiresAt > 0) }
    }.getOrNull()

    fun fromPush(data: Map<String, String>): AokieVoiceOffer? = runCatching {
      val allowed = setOf(
        "aokieClass", "schemaVersion", "eventId", "offerId", "appId", "callId",
        "callEpoch", "ownerEpoch", "expiresAt",
      )
      require(data.keys.all(allowed::contains))
      require(data["aokieClass"] == "voice_offer" && data["schemaVersion"] == "1")
      val now = System.currentTimeMillis() / 1000
      AokieVoiceOffer(
        eventId = checkedId(data.getValue("eventId")),
        offerId = checkedId(data.getValue("offerId")),
        appId = checkedId(data.getValue("appId")),
        callId = checkedId(data.getValue("callId")),
        callEpoch = checkedEpoch(data.getValue("callEpoch").toLong(), allowZero = false),
        ownerEpoch = checkedEpoch(data.getValue("ownerEpoch").toLong(), allowZero = true),
        expiresAt = data.getValue("expiresAt").toLong(),
      ).also {
        // A push is only a short opaque wake hint. Long-lived or already stale
        // payloads never create a call surface.
        require(it.expiresAt > now && it.expiresAt <= now + 5 * 60)
      }
    }.getOrNull()

    private fun checkedId(value: String): String {
      require(value.length in 1..200)
      require(value.all { it.isLetterOrDigit() || it in "-_.:" })
      return value
    }

    private fun checkedEpoch(value: Long, allowZero: Boolean): Long {
      val minimum = if (allowZero) 0 else 1
      require(value in minimum..9_007_199_254_740_991L)
      return value
    }
  }
}

internal object AokieOfferStore {
  private const val PENDING_KEY = "aokie.pending-voice-offer.v1"
  private const val DIAGNOSTIC_KEY = "aokie.runtime-diagnostic.v1"
  private val lock = Any()

  fun accept(context: Context, offer: AokieVoiceOffer): Boolean = synchronized(lock) {
    val current = current(context, clearExpired = true)
    if (current != null) {
      if (current.offerId == offer.offerId) return@synchronized current == offer
      // A later physical call epoch may replace a stale surface. Same/older
      // epochs must be cancelled by authoritative fan-out, never by a race.
      if (offer.callEpoch <= current.callEpoch) return@synchronized false
      AokieCallNotifications.cancelCall(context, current.offerId)
    }
    AokieSecureStore.put(context, PENDING_KEY, offer.toJson())
  }

  fun current(context: Context, clearExpired: Boolean = true): AokieVoiceOffer? = synchronized(lock) {
    val encoded = AokieSecureStore.get(context, PENDING_KEY) ?: return@synchronized null
    val offer = AokieVoiceOffer.fromJson(encoded)
    if (offer == null || (clearExpired && offer.isExpired())) {
      AokieSecureStore.delete(context, PENDING_KEY)
      offer?.let { AokieCallNotifications.cancelCall(context, it.offerId) }
      return@synchronized null
    }
    offer
  }

  fun markAnswerRequested(context: Context, offerId: String): Boolean = synchronized(lock) {
    val current = current(context, clearExpired = true) ?: return@synchronized false
    if (current.offerId != offerId || current.status != AokieVoiceOffer.STATUS_RINGING) {
      return@synchronized current.offerId == offerId && current.status == AokieVoiceOffer.STATUS_ANSWER_REQUESTED
    }
    AokieSecureStore.put(
      context,
      PENDING_KEY,
      current.withStatus(AokieVoiceOffer.STATUS_ANSWER_REQUESTED).toJson(),
    )
  }

  fun markWon(context: Context, callId: String, callEpoch: Long): AokieVoiceOffer? = synchronized(lock) {
    val current = current(context, clearExpired = true) ?: return@synchronized null
    if (current.callId != callId || current.callEpoch != callEpoch ||
      current.status == AokieVoiceOffer.STATUS_RINGING
    ) return@synchronized null
    val won = current.withStatus(AokieVoiceOffer.STATUS_WON)
    if (!AokieSecureStore.put(context, PENDING_KEY, won.toJson())) return@synchronized null
    won
  }

  fun cancel(context: Context, offerId: String?, reason: String): AokieVoiceOffer? = synchronized(lock) {
    val current = current(context, clearExpired = false) ?: return@synchronized null
    if (offerId != null && current.offerId != offerId) return@synchronized null
    AokieSecureStore.delete(context, PENDING_KEY)
    AokieCallNotifications.cancelCall(context, current.offerId)
    recordDiagnostic(context, reason)
    current
  }

  fun cancelByCall(context: Context, callId: String, callEpoch: Long, reason: String): AokieVoiceOffer? = synchronized(lock) {
    val current = current(context, clearExpired = false) ?: return@synchronized null
    if (current.callId != callId || current.callEpoch != callEpoch) return@synchronized null
    cancel(context, current.offerId, reason)
  }

  fun recordDiagnostic(context: Context, code: String) {
    val safe = code.take(120).filterNot(Char::isISOControl)
    val encoded = JSONObject()
      .put("code", safe)
      .put("occurredAt", System.currentTimeMillis() / 1000)
      .toString()
    AokieSecureStore.put(context, DIAGNOSTIC_KEY, encoded)
  }

  fun diagnostic(context: Context): String? = AokieSecureStore.get(context, DIAGNOSTIC_KEY)
}
