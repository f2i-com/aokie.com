package com.aokie.companion

import org.junit.Assert.assertEquals
import org.junit.Test

class AokieBackPolicyTest {
  /** Records what the policy asked of the activity. The interface has no way to finish it. */
  private class FakeHost(var canGoBack: Boolean) : BackHost {
    val calls = mutableListOf<String>()

    override fun pageCanGoBack(): Boolean = canGoBack

    override fun goBackInPage() {
      calls += "goBackInPage"
    }

    override fun moveTaskToBackground() {
      calls += "moveTaskToBackground"
    }
  }

  @Test
  fun backAtTheRootMovesTheTaskToTheBackgroundInsteadOfFinishing() {
    val host = FakeHost(canGoBack = false)
    handleBack(host)
    assertEquals(listOf("moveTaskToBackground"), host.calls)
  }

  @Test
  fun backGoesBackInThePageWhileThePageHasHistory() {
    val host = FakeHost(canGoBack = true)
    handleBack(host)
    assertEquals(listOf("goBackInPage"), host.calls)
  }

  @Test
  fun everyBackDoesExactlyOneThing() {
    val host = FakeHost(canGoBack = true)
    handleBack(host)
    host.canGoBack = false
    handleBack(host)
    handleBack(host)
    assertEquals(listOf("goBackInPage", "moveTaskToBackground", "moveTaskToBackground"), host.calls)
  }
}
