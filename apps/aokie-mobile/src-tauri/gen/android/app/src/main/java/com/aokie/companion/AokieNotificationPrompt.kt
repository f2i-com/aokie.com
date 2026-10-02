package com.aokie.companion

import android.content.Context

/**
 * What the app remembers about the notification question: whether Android has stopped showing it.
 *
 * After the user declines the system dialog twice, or declines it once and Settings fixed the choice, Android no
 * longer shows it: `requestPermissions` answers "denied" at once and the "Allow call notifications" button would do
 * nothing. The moment to learn that is the answer to a request: a denial that Android will not let the app follow up
 * with a rationale is final. The only way out is the app's notification settings, which the setup screen offers instead.
 */
internal object AokieNotificationPrompt {
  private const val PREFERENCES = "aokie_prompts"
  private const val DENIED_FOR_GOOD = "notification_denied_for_good"

  /** Called with the answer to a notification request. */
  fun recordAnswer(context: Context, granted: Boolean, canAskAgain: Boolean) {
    context.getSharedPreferences(PREFERENCES, Context.MODE_PRIVATE)
      .edit()
      .putBoolean(DENIED_FOR_GOOD, deniedForGood(granted, canAskAgain))
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

/** A denial is final when Android offers no further rationale to show. */
internal fun deniedForGood(granted: Boolean, canAskAgain: Boolean): Boolean = !granted && !canAskAgain

/** The notification permission is a runtime permission from Android 13 (API 33) on. */
internal fun promptBlocked(sdk: Int, granted: Boolean, deniedForGood: Boolean): Boolean =
  sdk >= 33 && !granted && deniedForGood
