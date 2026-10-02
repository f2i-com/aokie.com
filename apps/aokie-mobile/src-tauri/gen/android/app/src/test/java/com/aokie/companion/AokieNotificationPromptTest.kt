package com.aokie.companion

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class AokieNotificationPromptTest {
  @Test
  fun aFirstDenialCanStillBeFollowedUpWithAnotherRequest() {
    // Android 13 lets the app ask once more after the first denial: a rationale can still be shown.
    assertFalse(deniedForGood(granted = false, canAskAgain = true))
  }

  @Test
  fun aDenialWithNoRationaleLeftIsFinal() {
    // The second denial, or a choice that Settings fixed: Android shows no dialog any more.
    assertTrue(deniedForGood(granted = false, canAskAgain = false))
  }

  @Test
  fun anAllowedPermissionIsNeverFinal() {
    assertFalse(deniedForGood(granted = true, canAskAgain = false))
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
