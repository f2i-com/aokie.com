package com.aokie.companion

import android.content.Context
import android.os.SystemClock
import kotlinx.coroutines.delay
import org.json.JSONObject
import java.util.UUID

/**
 * Keystore-backed hand-off between Core-Telecom and the native Rust v2 client.
 *
 * The WebView never receives these records. An Answer record is completed once
 * the authoritative v2 lease request has been admitted to the live transport.
 * An End record is completed only after an exact lease-revoked acknowledgement,
 * or immediately when native v2 state proves that no matching lease exists.
 */
internal data class AokieNativeCallAction(
  val actionId: String,
  val kind: String,
  val offerId: String,
  val appId: String,
  val callId: String,
  val callEpoch: Long,
  val ownerEpoch: Long,
  val acceptedTransferRequestId: String?,
  val responseText: String?,
  val createdAt: Long,
) {
  fun toJson(): String = JSONObject()
    .put("schemaVersion", 2)
    .put("actionId", actionId)
    .put("kind", kind)
    .put("offerId", offerId)
    .put("appId", appId)
    .put("callId", callId)
    .put("callEpoch", callEpoch)
    .put("ownerEpoch", ownerEpoch)
    .put("acceptedTransferRequestId", acceptedTransferRequestId ?: JSONObject.NULL)
    .put("responseText", responseText ?: JSONObject.NULL)
    .put("createdAt", createdAt)
    .toString()

  companion object {
    const val ANSWER = "answer"
    const val DECLINE = "decline"
    const val END = "end"
    const val HANGUP = "hangup"
    private val kinds = setOf(ANSWER, DECLINE, END, HANGUP)

    fun create(
      offer: AokieVoiceOffer,
      kind: String,
      responseText: String? = null,
    ): AokieNativeCallAction {
      require(kind in kinds)
      require(kind != DECLINE || offer.acceptedTransferRequestId != null)
      require(kind == DECLINE || responseText == null)
      return AokieNativeCallAction(
        actionId = UUID.randomUUID().toString(),
        kind = kind,
        offerId = offer.offerId,
        appId = offer.appId,
        callId = offer.callId,
        callEpoch = offer.callEpoch,
        ownerEpoch = offer.ownerEpoch,
        acceptedTransferRequestId = offer.acceptedTransferRequestId.takeIf {
          kind == ANSWER || kind == DECLINE
        },
        responseText = responseText,
        createdAt = System.currentTimeMillis() / 1000,
      )
    }

    fun fromJson(encoded: String): AokieNativeCallAction? = runCatching {
      val json = JSONObject(encoded)
      require(json.length() == 11 && json.getInt("schemaVersion") == 2)
      AokieNativeCallAction(
        actionId = checkedId(json.getString("actionId")),
        kind = json.getString("kind").also { require(it in kinds) },
        offerId = checkedId(json.getString("offerId")),
        appId = checkedId(json.getString("appId")),
        callId = checkedId(json.getString("callId")),
        callEpoch = checkedEpoch(json.getLong("callEpoch"), allowZero = false),
        ownerEpoch = checkedEpoch(json.getLong("ownerEpoch"), allowZero = true),
        acceptedTransferRequestId = if (json.isNull("acceptedTransferRequestId")) null else
          checkedId(json.getString("acceptedTransferRequestId")),
        responseText = if (json.isNull("responseText")) null else
          json.getString("responseText").also {
            require(it.length in 1..500 && it.none(Char::isISOControl))
          },
        createdAt = json.getLong("createdAt").also {
          val now = System.currentTimeMillis() / 1000
          require(it in (now - 60)..(now + 5))
        },
      ).also {
        require(it.kind != DECLINE || (it.acceptedTransferRequestId != null && it.responseText != null))
        require(it.kind == DECLINE || it.responseText == null)
        require(it.kind !in setOf(END, HANGUP) || it.acceptedTransferRequestId == null)
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

internal data class AokieNativeCallResult(
  val actionId: String,
  val accepted: Boolean,
  val code: String,
)

internal object AokieNativeCallActionStore {
  private const val ACTION_KEY = "aokie.native-call-action.v1"
  private const val RESULT_KEY = "aokie.native-call-result.v1"
  private const val RESULT_TIMEOUT_MS = 6_000L
  // Desktop may wait through the bounded 8-10 second physical radio hangup
  // proof. Keep this beyond that bound without loosening Answer/Decline.
  private const val HANGUP_RESULT_TIMEOUT_MS = 15_000L
  private val lock = Any()

  fun enqueue(
    context: Context,
    offer: AokieVoiceOffer,
    kind: String,
    responseText: String? = null,
  ): String? = synchronized(lock) {
    val existing = AokieSecureStore.get(context, ACTION_KEY)
      ?.let(AokieNativeCallAction::fromJson)
    if (existing != null && existing.offerId == offer.offerId && existing.kind == kind) {
      return@synchronized existing.actionId
    }
    AokieSecureStore.delete(context, RESULT_KEY)
    val action = AokieNativeCallAction.create(offer, kind, responseText)
    if (AokieSecureStore.put(context, ACTION_KEY, action.toJson())) action.actionId else null
  }

  /** Atomically transfers ownership of the pending action to native Rust. */
  fun takeEncoded(context: Context): String? = synchronized(lock) {
    val encoded = AokieSecureStore.get(context, ACTION_KEY) ?: return@synchronized null
    if (!AokieSecureStore.delete(context, ACTION_KEY)) return@synchronized null
    AokieNativeCallAction.fromJson(encoded)?.toJson()
  }

  fun complete(context: Context, actionId: String, accepted: Boolean, code: String): Boolean = synchronized(lock) {
    if (!safeId(actionId) || !safeCode(code)) return@synchronized false
    val result = JSONObject()
      .put("schemaVersion", 1)
      .put("actionId", actionId)
      .put("accepted", accepted)
      .put("code", code)
      .put("completedAt", System.currentTimeMillis() / 1000)
      .toString()
    AokieSecureStore.put(context, RESULT_KEY, result)
  }

  suspend fun awaitResult(context: Context, actionId: String): AokieNativeCallResult? {
    return awaitResult(context, actionId, RESULT_TIMEOUT_MS)
  }

  suspend fun awaitHangupResult(context: Context, actionId: String): AokieNativeCallResult? {
    return awaitResult(context, actionId, HANGUP_RESULT_TIMEOUT_MS)
  }

  private suspend fun awaitResult(
    context: Context,
    actionId: String,
    timeoutMs: Long,
  ): AokieNativeCallResult? {
    val deadline = SystemClock.elapsedRealtime() + timeoutMs
    while (SystemClock.elapsedRealtime() < deadline) {
      val result = synchronized(lock) {
        val encoded = AokieSecureStore.get(context, RESULT_KEY) ?: return@synchronized null
        parseResult(encoded)?.takeIf { it.actionId == actionId }?.also {
          AokieSecureStore.delete(context, RESULT_KEY)
        }
      }
      if (result != null) return result
      delay(50)
    }
    synchronized(lock) {
      // Delete only a still-unclaimed request. A request already taken by Rust
      // may complete later, but its single result slot is harmlessly replaced
      // by the next action.
      val pending = AokieSecureStore.get(context, ACTION_KEY)
        ?.let(AokieNativeCallAction::fromJson)
      if (pending?.actionId == actionId) AokieSecureStore.delete(context, ACTION_KEY)
    }
    return null
  }

  private fun parseResult(encoded: String): AokieNativeCallResult? = runCatching {
    val json = JSONObject(encoded)
    require(json.length() == 5 && json.getInt("schemaVersion") == 1)
    val actionId = json.getString("actionId").also { require(safeId(it)) }
    val code = json.getString("code").also { require(safeCode(it)) }
    val completedAt = json.getLong("completedAt")
    val now = System.currentTimeMillis() / 1000
    require(completedAt in (now - 60)..(now + 5))
    AokieNativeCallResult(actionId, json.getBoolean("accepted"), code)
  }.getOrNull()

  private fun safeId(value: String): Boolean =
    value.length in 1..200 && value.all { it.isLetterOrDigit() || it in "-_.:" }

  private fun safeCode(value: String): Boolean =
    value.length in 1..120 && value.none(Char::isISOControl)
}
