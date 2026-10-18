package com.aokie.companion

import android.content.Context
import android.os.SystemClock
import android.provider.Settings
import org.json.JSONObject
import kotlin.math.abs
import kotlin.math.roundToLong

/**
 * The server-clock offset native Rust code checks protocol times with (`aokie_protocol::clock`), kept for the
 * pushes that wake this app before Rust runs (review 4 #10). A push's `expiresAt` is the server's time: checked
 * on this phone's own clock, a phone more than an offer's life ahead dropped every background call push, and a
 * cold start had no offset at all.
 *
 * Rust hands the offset over when it changes, and now and then while it holds. It is kept with when it came, on
 * the wall clock and on `elapsedRealtime` (which nobody sets and which counts deep sleep), and with the boot it
 * came in: within that boot a later step of the wall clock (network time putting it right, the owner setting it)
 * is measured and taken up, as Rust does. Across a reboot the offset is used as it was while it is recent.
 */
internal data class AokieServerClockRecord(
  val offsetSeconds: Long,
  val wallAtMs: Long,
  val elapsedAtMs: Long,
  val bootCount: Int?,
) {
  fun toJson(): String = JSONObject()
    .put("schemaVersion", 1)
    .put("offsetSeconds", offsetSeconds)
    .put("wallAtMs", wallAtMs)
    .put("elapsedAtMs", elapsedAtMs)
    .put("bootCount", bootCount ?: JSONObject.NULL)
    .toString()

  companion object {
    fun fromJson(encoded: String): AokieServerClockRecord? = runCatching {
      val json = JSONObject(encoded)
      require(json.getInt("schemaVersion") == 1 && json.length() == 5)
      AokieServerClockRecord(
        offsetSeconds = json.getLong("offsetSeconds"),
        wallAtMs = json.getLong("wallAtMs"),
        elapsedAtMs = json.getLong("elapsedAtMs"),
        bootCount = if (json.isNull("bootCount")) null else json.getInt("bootCount"),
      ).also {
        require(abs(it.offsetSeconds) <= AokieServerClock.MAX_OFFSET_SECONDS)
        require(it.wallAtMs > 0 && it.elapsedAtMs >= 0)
      }
    }.getOrNull()
  }
}

internal object AokieServerClock {
  const val KEY = "aokie.server-clock.v1"

  /** Native code's own bound: a larger offset is never taken. */
  const val MAX_OFFSET_SECONDS = 60L * 60

  /** Across a reboot, an offset older than this is not used. */
  const val REBOOT_STALE_MS = 7L * 24 * 60 * 60 * 1000

  fun record(context: Context, offsetSeconds: Long): Boolean {
    if (abs(offsetSeconds) > MAX_OFFSET_SECONDS) return false
    val record = AokieServerClockRecord(
      offsetSeconds = offsetSeconds,
      wallAtMs = System.currentTimeMillis(),
      elapsedAtMs = SystemClock.elapsedRealtime(),
      bootCount = bootCount(context),
    )
    return AokieSecureStore.put(context, KEY, record.toJson())
  }

  /** Seconds the server's clock is ahead of this phone's, now: 0 when native code has never said. */
  fun offsetSeconds(context: Context): Long {
    val record = runCatching { AokieSecureStore.get(context, KEY) }.getOrNull()
      ?.let(AokieServerClockRecord::fromJson)
      ?: return 0
    return aokieServerClockOffset(
      record,
      wallNowMs = System.currentTimeMillis(),
      elapsedNowMs = SystemClock.elapsedRealtime(),
      bootCount = bootCount(context),
    )
  }

  /** A server-clock time (Unix seconds) on this phone's own clock, which the push checks compare it with. */
  fun toLocalSeconds(context: Context, serverSeconds: Long): Long = serverSeconds - offsetSeconds(context)

  private fun bootCount(context: Context): Int? =
    runCatching { Settings.Global.getInt(context.contentResolver, Settings.Global.BOOT_COUNT) }.getOrNull()
}

/**
 * The offset [record] gives at [wallNowMs]/[elapsedNowMs] in boot [bootCount]. In the boot it was recorded in,
 * the wall clock's move against `elapsedRealtime` beyond rounding and drift (2 s, and 1,000 parts per million) is
 * a step, taken up as native code takes it up; a step past the bound is not followed. In another boot, or where
 * the boot cannot be told, the offset as recorded while it is recent, else none.
 */
internal fun aokieServerClockOffset(
  record: AokieServerClockRecord,
  wallNowMs: Long,
  elapsedNowMs: Long,
  bootCount: Int?,
): Long {
  val sameBoot = record.bootCount != null && record.bootCount == bootCount && elapsedNowMs >= record.elapsedAtMs
  if (!sameBoot) {
    return if (abs(wallNowMs - record.wallAtMs) <= AokieServerClock.REBOOT_STALE_MS) record.offsetSeconds else 0
  }
  val steadyMs = elapsedNowMs - record.elapsedAtMs
  val stepMs = (wallNowMs - record.wallAtMs) - steadyMs
  if (abs(stepMs) <= 2_000 + steadyMs / 1_000) return record.offsetSeconds
  val offset = record.offsetSeconds - (stepMs / 1_000.0).roundToLong()
  return if (abs(offset) <= AokieServerClock.MAX_OFFSET_SECONDS) offset else record.offsetSeconds
}
