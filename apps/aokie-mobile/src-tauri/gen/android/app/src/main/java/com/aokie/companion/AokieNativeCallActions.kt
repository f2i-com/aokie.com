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
  val createdAt: Long,
) {
  fun toJson(): String = JSONObject()
    .put("schemaVersion", 1)
    .put("actionId", actionId)
    .put("kind", kind)
    .put("offerId", offerId)
    .put("appId", appId)
    .put("callId", callId)
    .put("callEpoch", callEpoch)
    .put("ownerEpoch", ownerEpoch)
    .put("createdAt", createdAt)
    .toString()

  companion object {
    const val ANSWER = "answer"
    const val END = "end"
    private val kinds = setOf(ANSWER, END)

    fun create(offer: AokieVoiceOffer, kind: String): AokieNativeCallAction {
      require(kind in kinds)
      return AokieNativeCallAction(
        actionId = UUID.randomUUID().toString(),
        kind = kind,
        offerId = offer.offerId,
        appId = offer.appId,
        callId = offer.callId,
        callEpoch = offer.callEpoch,
        ownerEpoch = offer.ownerEpoch,
        createdAt = System.currentTimeMillis() / 1000,
      )
    }

    fun fromJson(encoded: String): AokieNativeCallAction? = runCatching {
      val json = JSONObject(encoded)
      require(json.length() == 9 && json.getInt("schemaVersion") == 1)
      AokieNativeCallAction(
        actionId = checkedId(json.getString("actionId")),
        kind = json.getString("kind").also { require(it in kinds) },
        offerId = checkedId(json.getString("offerId")),
        appId = checkedId(json.getString("appId")),
        callId = checkedId(json.getString("callId")),
        callEpoch = checkedEpoch(json.getLong("callEpoch"), allowZero = false),
        ownerEpoch = checkedEpoch(json.getLong("ownerEpoch"), allowZero = true),
        createdAt = json.getLong("createdAt").also {
          val now = System.currentTimeMillis() / 1000
          require(it in (now - 60)..(now + 5))
        },
      )
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
  private val lock = Any()

  fun enqueue(context: Context, offer: AokieVoiceOffer, kind: String): String? = synchronized(lock) {
    val existing = AokieSecureStore.get(context, ACTION_KEY)
      ?.let(AokieNativeCallAction::fromJson)
    if (existing != null && existing.offerId == offer.offerId && existing.kind == kind) {
      return@synchronized existing.actionId
    }
    AokieSecureStore.delete(context, RESULT_KEY)
    val action = AokieNativeCallAction.create(offer, kind)
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
    val deadline = SystemClock.elapsedRealtime() + RESULT_TIMEOUT_MS
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
