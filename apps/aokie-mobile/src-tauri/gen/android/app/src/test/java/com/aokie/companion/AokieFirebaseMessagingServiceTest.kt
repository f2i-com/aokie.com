package com.aokie.companion

import android.telecom.DisconnectCause
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AokieFirebaseMessagingServiceTest {
  @Test
  fun voiceOfferCancellationAcceptsOnlyItsExactPushClass() {
    val valid = mapOf(
      "aokieClass" to "voice_offer_cancel",
      "schemaVersion" to "1",
      "eventId" to "event_a",
      "offerId" to "offer_a",
      "reason" to "authoritative_replacement",
    )
    assertEquals(
      AokieVoiceOfferCancellation("offer_a", "authoritative_replacement"),
      parseVoiceOfferCancel(valid),
    )
    assertNull(parseVoiceOfferCancel(valid + ("aokieClass" to "assistance_offer")))
    assertNull(parseVoiceOfferCancel(valid + ("unexpected" to "authority")))
  }

  @Test
  fun onlyTheAuthenticatedTransferIntentRequestsScreenWake() {
    assertEquals(
      true,
      shouldWakeForAuthoritativeTransfer(MainActivity.ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE),
    )
    assertEquals(false, shouldWakeForAuthoritativeTransfer(null))
    assertEquals(false, shouldWakeForAuthoritativeTransfer("android.intent.action.MAIN"))
  }

  @Test
  fun nearExpiryAnswerGetsBoundedSetupTimeAndWonOutlivesTheOfferDeadline() {
    val fence = AokieCallFence("offer_a", 1)
    val ringing = AokieVoiceOffer(
      eventId = "event_a",
      offerId = "offer_a",
      opportunityId = "opportunity_a",
      appId = "app_a",
      callId = "call_a",
      callEpoch = 1,
      ownerEpoch = 0,
      expiresAt = 101,
    )
    val answerRequested = ringing.answerRequested(nowSeconds = 100)

    assertFalse(ringing.isExpired(nowSeconds = 100))
    assertFalse(answerRequested.isExpired(nowSeconds = 101))
    assertFalse(answerRequested.isExpired(nowSeconds = 144))
    assertTrue(answerRequested.isExpired(nowSeconds = 145))
    assertFalse(
      aokieOfferExpiryApplies(
        fence,
        fence,
        actionDeadlineAt = 101,
        currentOfferId = "offer_a",
        currentStatus = AokieVoiceOffer.STATUS_ANSWER_REQUESTED,
        currentDeadlineAt = 145,
        nowSeconds = 101,
      ),
    )
    assertTrue(
      aokieOfferExpiryApplies(
        fence,
        fence,
        actionDeadlineAt = 145,
        currentOfferId = "offer_a",
        currentStatus = AokieVoiceOffer.STATUS_ANSWER_REQUESTED,
        currentDeadlineAt = 145,
        nowSeconds = 145,
      ),
    )
    val won = answerRequested.won()
    assertFalse(won.isExpired(nowSeconds = 10_000))
    assertFalse(
      aokieOfferExpiryApplies(
        fence,
        fence,
        actionDeadlineAt = 145,
        currentOfferId = "offer_a",
        currentStatus = AokieVoiceOffer.STATUS_WON,
        currentDeadlineAt = won.expiryDeadlineAt(),
        nowSeconds = 145,
      ),
    )
  }

  @Test
  fun queuedActionsFromAnOldCallGenerationCannotAffectItsReplacement() {
    val old = AokieCallFence("offer_a", 1)
    val replacement = AokieCallFence("offer_b", 2)

    assertFalse(aokieCallActionApplies(old, replacement))
    assertFalse(
      aokieOfferExpiryApplies(
        old,
        replacement,
        actionDeadlineAt = 100,
        currentOfferId = "offer_b",
        currentStatus = AokieVoiceOffer.STATUS_RINGING,
        currentDeadlineAt = 100,
        nowSeconds = 100,
      ),
    )
    assertTrue(aokieCallActionApplies(replacement, replacement))
  }

  @Test
  fun onlyExplicitLocalEndOnAWonCallCanRequestCallerHangup() {
    assertEquals(
      AokieSystemEndRoute.HANG_UP_CALLER,
      aokieSystemEndRoute(AokieVoiceOffer.STATUS_WON, DisconnectCause.LOCAL),
    )
    assertEquals(
      AokieSystemEndRoute.RETURN_TO_AOKIE,
      aokieSystemEndRoute(AokieVoiceOffer.STATUS_WON, DisconnectCause.ERROR),
    )
    assertEquals(
      AokieSystemEndRoute.RETURN_TO_AOKIE,
      aokieSystemEndRoute(AokieVoiceOffer.STATUS_WON, DisconnectCause.CANCELED),
    )
    assertEquals(
      AokieSystemEndRoute.DECLINE_RINGING,
      aokieSystemEndRoute(AokieVoiceOffer.STATUS_RINGING, DisconnectCause.REJECTED),
    )
    assertEquals(
      AokieSystemEndRoute.DECLINE_RINGING,
      aokieSystemEndRoute(AokieVoiceOffer.STATUS_RINGING, DisconnectCause.LOCAL),
    )
    assertEquals(
      AokieSystemHangupResultRoute.CALLER_ENDED,
      aokieSystemHangupResultRoute(confirmed = true),
    )
    assertEquals(
      AokieSystemHangupResultRoute.RETURN_COMPANION_LEASE,
      aokieSystemHangupResultRoute(confirmed = false),
    )
  }
}
