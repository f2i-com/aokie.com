package com.aokie.companion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test

class AokieServerClockTest {
  private val recorded = AokieServerClockRecord(
    offsetSeconds = -60,
    wallAtMs = 1_800_000_000_000,
    elapsedAtMs = 5_000_000,
    bootCount = 12,
  )

  @Test
  fun aPushFromAServerBehindThisPhoneIsTakenOnTheServersClock() {
    // Review 4 #10: a phone a minute fast, and a push the server sent now for a 45 s offer.
    val serverNow = 1_800_000_000L
    val phoneNow = serverNow + 60
    val push = mapOf(
      "aokieClass" to "voice_offer",
      "schemaVersion" to "1",
      "eventId" to "event_a",
      "offerId" to "offer_a",
      "appId" to "app_a",
      "callId" to "call_a",
      "callEpoch" to "1",
      "ownerEpoch" to "0",
      "expiresAt" to (serverNow + 45).toString(),
    )
    assertNull("on its own clock the phone dropped it", AokieVoiceOffer.fromPush(push, 0, phoneNow))
    val offer = AokieVoiceOffer.fromPush(push, -60, phoneNow)
    assertNotNull(offer)
    assertEquals("kept on this phone's clock", phoneNow + 45, offer!!.expiresAt)
    assertNull("an offset past the bound is refused", AokieVoiceOffer.fromPush(push, 3_601, phoneNow))
  }

  @Test
  fun theOffsetIsUsedAsRecordedUntilTheWallClockIsSet() {
    // An hour later, asleep for most of it: no step.
    assertEquals(-60, aokieServerClockOffset(recorded, recorded.wallAtMs + 3_600_000, recorded.elapsedAtMs + 3_600_000, 12))
    // Whole-second rounding and drift are not a step.
    assertEquals(-60, aokieServerClockOffset(recorded, recorded.wallAtMs + 3_601_500, recorded.elapsedAtMs + 3_600_000, 12))
    // The fast clock put right a minute later: in step with the server now.
    assertEquals(0, aokieServerClockOffset(recorded, recorded.wallAtMs + 60_000 - 60_000, recorded.elapsedAtMs + 60_000, 12))
    // Set an hour and more the other way: past the bound, not followed.
    assertEquals(-60, aokieServerClockOffset(recorded, recorded.wallAtMs + 2 * 3_600_000, recorded.elapsedAtMs + 1_000, 12))
  }

  @Test
  fun anotherBootUsesTheRecordedOffsetWhileItIsRecent() {
    assertEquals(-60, aokieServerClockOffset(recorded, recorded.wallAtMs + 86_400_000, 30_000, 13))
    assertEquals(0, aokieServerClockOffset(recorded, recorded.wallAtMs + AokieServerClock.REBOOT_STALE_MS + 1, 30_000, 13))
    // A boot that cannot be told is taken as another.
    assertEquals(-60, aokieServerClockOffset(recorded.copy(bootCount = null), recorded.wallAtMs + 1_000, recorded.elapsedAtMs + 1_000, null))
  }
}
