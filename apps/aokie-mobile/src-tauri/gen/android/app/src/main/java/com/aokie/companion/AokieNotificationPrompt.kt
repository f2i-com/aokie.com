package com.aokie.companion

import android.content.Context

/**
 * What the app remembers about the notification question: whether Android has stopped showing it.
 *
 * When the choice is fixed (the user declined twice, or Settings fixed it) Android shows no dialog: `requestPermissions`
 * answers "denied" by itself and the "Allow call notifications" button would do nothing. The only way out is the
 * app's notification settings, which the setup screen offers instead. [answerIsFinal] tells that case from the
 * ones where the dialog can still be shown.
 */
internal object AokieNotificationPrompt {
  private const val PREFERENCES = "aokie_prompts"
  private const val DENIED_FOR_GOOD = "notification_denied_for_good"

  /** Called with what the answer to a notification request means; see [answerIsFinal]. */
  fun recordAnswer(context: Context, final: Boolean) {
    context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .putBoolean(DENIED_FOR_GOOD, final)
      .apply()
  }

  /** True while Android will not show the question again. Forgotten as soon as notifications are allowed. */
  fun blocked(context: Context, sdk: Int, granted: Boolean): Boolean {
    val preferences = context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
    val remembered = preferences.getBoolean(DENIED_FOR_GOOD, false)
    if (granted && remembered) {
      preferences.edit().putBoolean(DENIED_FOR_GOOD, false).apply()
    }
    return promptBlocked(sdk, granted, remembered)
  }
}

/** Android answers a request for a fixed choice within a few hundred milliseconds, with no one to read a dialog. */
internal const val ANSWERED_BY_ANDROID_WITHIN_MS = 600L

/**
 * Whether the answer to a notification request means Android will not show its question again.
 *
 * Android's own signal is `shouldShowRequestPermissionRationale`: false before the first request, true after a first
 * denial, false again once the choice is fixed. That alone cannot tell a fixed choice from a dialog the user closed with
 * Back or by tapping outside, which Android does not count as a denial and which leaves the rationale as it was. So:
 *
 * - allowed, or a rationale is still available afterwards: the question can be asked again;
 * - a rationale was available before and is gone now: the user declined for the second time, the choice is fixed;
 * - none before and none after: either the user dismissed the dialog, or Android refused at once because the choice was
 *   already fixed. Android answers at once, a person does not: only a fast answer is final.
 */
internal fun answerIsFinal(
  granted: Boolean,
  canAskAgainBefore: Boolean,
  canAskAgainAfter: Boolean,
  answeredAfterMs: Long,
): Boolean = when {
  granted || canAskAgainAfter -> false
  canAskAgainBefore -> true
  else -> answeredAfterMs < ANSWERED_BY_ANDROID_WITHIN_MS
}

/** The notification permission is a runtime permission from Android 13 (API 33) on. */
internal fun promptBlocked(sdk: Int, granted: Boolean, deniedForGood: Boolean): Boolean =
  sdk >= 33 && !granted && deniedForGood
