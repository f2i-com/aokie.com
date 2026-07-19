package com.aokie.companion

import android.content.Context
import org.json.JSONObject

internal data class AokieVoiceOffer(
  val eventId: String,
  val offerId: String,
  val opportunityId: String,
  val appId: String,
  val callId: String,
  val callEpoch: Long,
  val ownerEpoch: Long,
  val expiresAt: Long,
  val acceptedTransferRequestId: String? = null,
  val authoritative: Boolean = false,
  val status: String = STATUS_RINGING,
  val answerDeadlineAt: Long? = null,
) {
  fun expiryDeadlineAt(): Long? = when (status) {
    STATUS_RINGING -> expiresAt
    STATUS_ANSWER_REQUESTED -> answerDeadlineAt ?: expiresAt
    STATUS_WON -> null
    else -> expiresAt
  }

  fun isExpired(nowSeconds: Long = System.currentTimeMillis() / 1000): Boolean =
    expiryDeadlineAt()?.let { it <= nowSeconds } ?: false

  fun toJson(): String = JSONObject()
    .put("schemaVersion", 3)
    .put("eventId", eventId)
    .put("offerId", offerId)
    .put("opportunityId", opportunityId)
    .put("appId", appId)
    .put("callId", callId)
    .put("callEpoch", callEpoch)
    .put("ownerEpoch", ownerEpoch)
    .put("expiresAt", expiresAt)
    .put("acceptedTransferRequestId", acceptedTransferRequestId ?: JSONObject.NULL)
    .put("authoritative", authoritative)
    .put("status", status)
    .put("answerDeadlineAt", answerDeadlineAt ?: JSONObject.NULL)
    .toString()

  fun answerRequested(nowSeconds: Long = System.currentTimeMillis() / 1000): AokieVoiceOffer =
    copy(
      status = STATUS_ANSWER_REQUESTED,
      answerDeadlineAt = nowSeconds + ANSWER_SETUP_TIMEOUT_SECONDS,
    )

  fun won(): AokieVoiceOffer = copy(status = STATUS_WON, answerDeadlineAt = null)

  companion object {
    const val STATUS_RINGING = "ringing"
    const val STATUS_ANSWER_REQUESTED = "answer_requested"
    const val STATUS_WON = "won"
    const val ANSWER_SETUP_TIMEOUT_SECONDS = 45L
    private val statuses = setOf(STATUS_RINGING, STATUS_ANSWER_REQUESTED, STATUS_WON)

    fun fromJson(encoded: String): AokieVoiceOffer? = runCatching {
      val json = JSONObject(encoded)
      val schemaVersion = json.getInt("schemaVersion")
      require(
        (schemaVersion == 2 && json.length() == 12) ||
          (schemaVersion == 3 && json.length() == 13),
      )
      AokieVoiceOffer(
        eventId = checkedId(json.getString("eventId")),
        offerId = checkedId(json.getString("offerId")),
        opportunityId = checkedId(json.getString("opportunityId")),
        appId = checkedId(json.getString("appId")),
        callId = checkedId(json.getString("callId")),
        callEpoch = checkedEpoch(json.getLong("callEpoch"), allowZero = false),
        ownerEpoch = checkedEpoch(json.getLong("ownerEpoch"), allowZero = true),
        expiresAt = json.getLong("expiresAt"),
        acceptedTransferRequestId = if (json.isNull("acceptedTransferRequestId")) null else
          checkedId(json.getString("acceptedTransferRequestId")),
        authoritative = json.getBoolean("authoritative"),
        status = json.getString("status").also { require(it in statuses) },
        answerDeadlineAt = when {
          schemaVersion == 3 && !json.isNull("answerDeadlineAt") ->
            json.getLong("answerDeadlineAt")
          schemaVersion == 2 && json.getString("status") == STATUS_ANSWER_REQUESTED ->
            json.getLong("expiresAt")
          else -> null
        },
      ).also {
        require(it.expiresAt > 0)
        require(
          (it.status == STATUS_ANSWER_REQUESTED) == (it.answerDeadlineAt != null),
        )
        require(it.answerDeadlineAt == null || it.answerDeadlineAt > 0)
      }
    }.getOrNull()

    fun fromAuthoritativeJson(encoded: String): AokieVoiceOffer? = runCatching {
      val json = JSONObject(encoded)
      val allowed = setOf(
        "schemaVersion", "eventId", "offerId", "opportunityId", "appId", "callId",
        "callEpoch", "ownerEpoch", "expiresAt", "acceptedTransferRequestId",
      )
      require(json.keys().asSequence().all(allowed::contains))
      require(json.length() == 10 && json.getInt("schemaVersion") == 1)
      AokieVoiceOffer(
        eventId = checkedId(json.getString("eventId")),
        offerId = checkedId(json.getString("offerId")),
        opportunityId = checkedId(json.getString("opportunityId")),
        appId = checkedId(json.getString("appId")),
        callId = checkedId(json.getString("callId")),
        callEpoch = checkedEpoch(json.getLong("callEpoch"), allowZero = false),
        ownerEpoch = checkedEpoch(json.getLong("ownerEpoch"), allowZero = true),
        expiresAt = json.getLong("expiresAt"),
        acceptedTransferRequestId = if (json.isNull("acceptedTransferRequestId")) null else
          checkedId(json.getString("acceptedTransferRequestId")),
        authoritative = true,
      ).also {
        val now = System.currentTimeMillis() / 1000
        require(it.expiresAt > now && it.expiresAt <= now + 5 * 60)
      }
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
        // Push is a wake hint, not the signed offer. Its opaque offer id is
        // used as the placeholder opportunity until authenticated v2 state
        // upgrades this record.
        opportunityId = checkedId(data.getValue("offerId")),
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

  fun acceptPush(context: Context, offer: AokieVoiceOffer): Boolean = synchronized(lock) {
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

  /**
   * Publishes only an offer reconstructed from authenticated, signed v2 state.
   * A matching opaque push placeholder is upgraded in place while preserving
   * the user's system-surface state. A genuinely new signed offer replaces the
   * previous ringing surface and the call service reconciles that replacement.
   */
  fun acceptAuthoritative(context: Context, offer: AokieVoiceOffer): AokieVoiceOffer? = synchronized(lock) {
    if (!offer.authoritative || offer.isExpired()) return@synchronized null
    val current = current(context, clearExpired = true)
    val committed = if (current?.offerId == offer.offerId) {
      if (current.appId != offer.appId || current.callId != offer.callId ||
        current.callEpoch != offer.callEpoch || current.ownerEpoch != offer.ownerEpoch
      ) return@synchronized null
      offer.copy(
        status = current.status,
        answerDeadlineAt = current.answerDeadlineAt,
      )
    } else {
      offer
    }
    if (!AokieSecureStore.put(context, PENDING_KEY, committed.toJson())) return@synchronized null
    if (current != null && current.offerId != committed.offerId) {
      AokieCallNotifications.cancelCall(context, current.offerId)
    }
    committed
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

  fun markAnswerRequested(context: Context, offerId: String): AokieVoiceOffer? = synchronized(lock) {
    val current = current(context, clearExpired = true) ?: return@synchronized null
    if (current.offerId != offerId || current.status != AokieVoiceOffer.STATUS_RINGING) {
      return@synchronized current.takeIf {
        it.offerId == offerId && it.status == AokieVoiceOffer.STATUS_ANSWER_REQUESTED
      }
    }
    val answerRequested = current.answerRequested()
    if (!AokieSecureStore.put(
      context,
      PENDING_KEY,
      answerRequested.toJson(),
    )) return@synchronized null
    answerRequested
  }

  fun markWon(context: Context, callId: String, callEpoch: Long): AokieVoiceOffer? = synchronized(lock) {
    val current = current(context, clearExpired = true) ?: return@synchronized null
    if (current.callId != callId || current.callEpoch != callEpoch ||
      current.status == AokieVoiceOffer.STATUS_RINGING
    ) return@synchronized null
    val won = current.won()
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
