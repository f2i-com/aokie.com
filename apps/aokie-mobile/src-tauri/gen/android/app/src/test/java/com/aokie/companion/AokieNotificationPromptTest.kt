package com.aokie.companion

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AokieNotificationPromptTest {
  private fun final(granted: Boolean, before: Boolean, after: Boolean, ms: Long) =
    answerIsFinal(granted = granted, canAskAgainBefore = before, canAskAgainAfter = after, answeredAfterMs = ms)

  @Test
  fun aFirstDenialCanStillBeFollowedUpWithAnotherRequest() {
    // Android 13 lets the app ask once more after the first denial: a rationale can now be shown.
    assertFalse(final(granted = false, before = false, after = true, ms = 4_000))
  }

  @Test
  fun theSecondDenialIsFinal() {
    // A rationale was available before the request and is gone after it.
    assertTrue(final(granted = false, before = true, after = false, ms = 4_000))
  }

  @Test
  fun aDialogClosedWithBackOrByTappingOutsideIsNotFinal() {
    // Android does not count this as a denial: no rationale before, none after, and a person took seconds.
    assertFalse(final(granted = false, before = false, after = false, ms = 3_500))
    // The same on the second request, when the rationale is still there afterwards.
    assertFalse(final(granted = false, before = true, after = true, ms = 3_500))
  }

  @Test
  fun aRefusalAndroidGivesAtOnceIsFinal() {
    // The choice was already fixed: no rationale before or after, and the answer came without anyone reading a dialog.
    assertTrue(final(granted = false, before = false, after = false, ms = 250))
    assertTrue(final(granted = false, before = false, after = false, ms = ANSWERED_BY_ANDROID_WITHIN_MS - 1))
    assertFalse(final(granted = false, before = false, after = false, ms = ANSWERED_BY_ANDROID_WITHIN_MS))
  }

  @Test
  fun anAllowedPermissionIsNeverFinal() {
    assertFalse(final(granted = true, before = true, after = false, ms = 100))
    assertFalse(final(granted = true, before = false, after = false, ms = 100))
  }

  @Test
  fun theQuestionIsOnlyBlockedWhereNotificationsAreARuntimePermission() {
    assertTrue(promptBlocked(sdk = 33, granted = false, deniedForGood = true))
    assertTrue(promptBlocked(sdk = 36, granted = false, deniedForGood = true))
    // Before Android 13 there is no question to refuse.
    assertFalse(promptBlocked(sdk = 32, granted = false, deniedForGood = true))
    // Nothing is blocked once notifications are allowed, or while the question may still be asked.
    assertFalse(promptBlocked(sdk = 36, granted = true, deniedForGood = true))
    assertFalse(promptBlocked(sdk = 36, granted = false, deniedForGood = false))
  }
}
