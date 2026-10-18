package com.aokie.companion

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AokieOfferStoreTest {
  private fun offer(
    offerId: String,
    callId: String = "call_a",
    callEpoch: Long = 5,
    expiresAt: Long = 1_000,
    appId: String = "app_a",
  ) = AokieVoiceOffer(
    eventId = "event_$offerId",
    offerId = offerId,
    opportunityId = "opportunity_$offerId",
    appId = appId,
    callId = callId,
    callEpoch = callEpoch,
    ownerEpoch = 0,
    expiresAt = expiresAt,
  )

  @Test
  fun aWonCallThisProcessRunsIsNeverReplacedOrCancelledByAPush() {
    val won = offer("offer_a").answerRequested(nowSeconds = 900).won()
    val later = offer("offer_b", callId = "call_b", callEpoch = 99, expiresAt = 2_000)

    assertFalse(aokieOfferOrphaned(won.status, won.offerId, liveOfferId = "offer_a"))
    assertFalse(aokiePushMayReplace(won, later, liveOfferId = "offer_a"))
    assertFalse(aokiePushMayCancel(won, liveOfferId = "offer_a"))

    val answering = offer("offer_a").answerRequested(nowSeconds = 900)
    assertFalse(aokiePushMayReplace(answering, later, liveOfferId = "offer_a"))
    assertFalse(aokiePushMayCancel(answering, liveOfferId = "offer_a"))
  }

  @Test
  fun aWonRecordLeftByADeadProcessGivesWayEvenToARestartedDesktopsLowerEpoch() {
    // The process that ran the call died: no call service runs it, so the record is an orphan.
    val won = offer("offer_a", callEpoch = 40).answerRequested(nowSeconds = 900).won()
    val afterRestart = offer("offer_b", callId = "call_b", callEpoch = 1, expiresAt = 2_000)

    assertTrue(aokieOfferOrphaned(won.status, won.offerId, liveOfferId = null))
    assertTrue(aokieOfferOrphaned(won.status, won.offerId, liveOfferId = "offer_other"))
    assertTrue(aokiePushMayReplace(won, afterRestart, liveOfferId = null))
    assertTrue(aokiePushMayCancel(won, liveOfferId = null))
  }

  @Test
  fun aRingingOfferIsNeverAnOrphanAndFollowsTheEpochAndExpiryRules() {
    val ringing = offer("offer_a", callEpoch = 5, expiresAt = 1_000)
    assertFalse(aokieOfferOrphaned(ringing.status, ringing.offerId, liveOfferId = null))
    assertTrue(aokiePushMayCancel(ringing, liveOfferId = null))

    // The same call: only a later epoch replaces it, never a race of the same or an older one.
    assertTrue(aokiePushMayReplace(ringing, offer("offer_b", callEpoch = 6), liveOfferId = null))
    assertFalse(aokiePushMayReplace(ringing, offer("offer_b", callEpoch = 5, expiresAt = 2_000), liveOfferId = null))
    assertFalse(aokiePushMayReplace(ringing, offer("offer_b", callEpoch = 4, expiresAt = 2_000), liveOfferId = null))

    // A different call: a later epoch, or a later expiry after a Desktop restart reset the epochs.
    assertTrue(aokiePushMayReplace(ringing, offer("offer_c", callId = "call_c", callEpoch = 1, expiresAt = 1_100), liveOfferId = null))
    assertFalse(aokiePushMayReplace(ringing, offer("offer_c", callId = "call_c", callEpoch = 1, expiresAt = 900), liveOfferId = null))
    assertTrue(aokiePushMayReplace(ringing, offer("offer_d", appId = "app_b", callEpoch = 1, expiresAt = 1_100), liveOfferId = null))

    // The same offer is never a replacement of itself.
    assertFalse(aokiePushMayReplace(ringing, ringing, liveOfferId = null))
  }

  @Test
  fun anAnswerWaitsForTheSignedOfferUntilItsOwnAnswerDeadlineNotAFixedFiveSeconds() {
    val answering = offer("offer_a", expiresAt = 2_000).answerRequested(nowSeconds = 900)
    // Pressed just now: the whole 45 s claim window.
    assertEquals(45_000L, aokieAuthoritativeOfferWaitMs(answering, nowMs = 900_000))
    // Twelve seconds of a slow cold start already gone: the rest of the window.
    assertEquals(33_000L, aokieAuthoritativeOfferWaitMs(answering, nowMs = 912_000))
    // Past the deadline: no wait at all, and never negative.
    assertEquals(0L, aokieAuthoritativeOfferWaitMs(answering, nowMs = 950_000))
    // A clock set back mid-call never stretches it past the window.
    assertEquals(45_000L, aokieAuthoritativeOfferWaitMs(answering, nowMs = 100_000))
    // A decline on a ringing offer, or no offer, keeps the short wait.
    assertEquals(AOKIE_AUTHORITATIVE_OFFER_WAIT_MS, aokieAuthoritativeOfferWaitMs(offer("offer_b"), nowMs = 900_000))
    assertEquals(AOKIE_AUTHORITATIVE_OFFER_WAIT_MS, aokieAuthoritativeOfferWaitMs(null, nowMs = 900_000))
  }
}
