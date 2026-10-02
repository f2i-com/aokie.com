package com.aokie.companion

/**
 * What the Back button does in [MainActivity].
 *
 * Back never finishes the activity. Finishing it destroys the WebView, and Tauri's Android runtime ends with
 * `process::exit` on the destroy: libc then runs the static destructors of every loaded library while the
 * process's render thread is still tearing the WebView down, and Android's libhwui aborts the process with
 * "FORTIFY: pthread_mutex_lock called on a destroyed mutex" (`WebViewFunctorManager::destroyFunctor`). On the
 * emulator that happened after nearly every Back at the first screen. Moving the task to the background keeps
 * the activity, the runtime and a running call service as they are, which is also what Home does and what
 * Android 12 and later do for the root activity of a launcher task.
 *
 * The page keeps its own history first: Back goes back in the page while it can.
 */
internal interface BackHost {
  /** The page in the WebView has an earlier entry in its history. */
  fun pageCanGoBack(): Boolean

  fun goBackInPage()

  /** `Activity.moveTaskToBack(true)`: the task leaves the screen, nothing is destroyed. */
  fun moveTaskToBackground()
}

/** Applies the Back policy to [host]. Deliberately has no way to finish the activity. */
internal fun handleBack(host: BackHost) {
  if (host.pageCanGoBack()) host.goBackInPage() else host.moveTaskToBackground()
}
