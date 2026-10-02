//! How the Android process ends once its activity is gone.
//!
//! When the last activity is destroyed, Tauri's event loop ends and tao calls `std::process::exit`. That is
//! libc's `exit`, which runs the static destructors of every library in the process while the process is still
//! live: Android's own `libhwui` render thread is then tearing the WebView down, locks a mutex that its library
//! destructor has just destroyed, and bionic aborts with "FORTIFY: pthread_mutex_lock called on a destroyed
//! mutex" (`WebViewFunctorManager::destroyFunctor`). The crash was visible after the activity had finished, so the
//! user saw nothing, but Android recorded a native crash of the app.
//!
//! The process has nothing left to flush at that point, so it ends with `_exit`, which runs no destructors and
//! leaves the render thread nothing to trip over.

/// Ends the process at once, without running atexit handlers or static destructors.
#[cfg(target_os = "android")]
pub(crate) fn exit_now() -> ! {
    extern "C" {
        fn _exit(status: core::ffi::c_int) -> !;
    }
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { _exit(0) }
}
