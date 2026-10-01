package com.aokie.companion

import android.content.Context
import android.util.Log

/**
 * Native start-up that has to happen inside a Java-to-native call, before the Rust runtime runs.
 *
 * JNI's `FindClass` resolves a class through the class loader of the Java frame that called into
 * native code. Rust code that Tauri runs from the Looper (a command handler, a webview callback)
 * has no such frame, so a lookup there uses the boot class loader and cannot see the app's
 * classes. libwebrtc looks its own Java runtime up by name (`livekit/org/jni_zero/JniZero`,
 * `livekit/org/webrtc/ContextUtils`), so initialising it from the Looper aborts the process with
 * "Class not found using the boot class loader". Calling it from `external fun`s below gives it
 * the app's class loader.
 *
 * Call [initialize] from `MainActivity.onCreate` before `super.onCreate`, which is what starts the
 * Rust runtime. Loading the native library here is harmless: Tauri loads the same library and a
 * second `loadLibrary` is a no-op.
 */
object AokieNativeInit {
  private const val TAG = "AokieNativeInit"
  private const val LIBRARY = "aokie_mobile_lib"

  @Volatile
  private var attempted = false

  /** Runs each step once per process. A failed step is logged and leaves its feature unavailable. */
  @Synchronized
  fun initialize(context: Context) {
    if (attempted) return
    attempted = true
    val application = context.applicationContext
    try {
      System.loadLibrary(LIBRARY)
    } catch (error: Throwable) {
      Log.e(TAG, "the native library could not be loaded", error)
      return
    }
    Log.i(TAG, "platform verifier: ${step("platform verifier") { nativeInitPlatformVerifier(application) }}")
    Log.i(TAG, "webrtc context: ${step("webrtc context") { nativeInitWebRtc(application) }}")
  }

  private fun step(what: String, block: () -> Boolean): Boolean =
    try {
      block()
    } catch (error: Throwable) {
      Log.e(TAG, "$what initialisation failed", error)
      false
    }

  /**
   * Implemented in Rust (`native_init.rs`). Gives `rustls-platform-verifier`, the certificate
   * verifier behind every HTTPS client in the Rust core, the application Context it needs before
   * its first handshake. Without it every HTTPS request fails.
   */
  @JvmStatic
  external fun nativeInitPlatformVerifier(context: Context): Boolean

  /** Implemented in Rust (`native_init.rs`). Initialises libwebrtc's Java side. */
  @JvmStatic
  external fun nativeInitWebRtc(context: Context): Boolean
}
