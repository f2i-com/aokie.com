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
//! leaves the render thread nothing to trip over. The one thing it does first is hand back a live Companion
//! lease (`return_companion_lease_before_exit` in lib.rs): swiping the app away from Recents mid-call ends
//! here too, and the Desktop must take the caller back rather than wait on a phone that is gone.

/// Ends the process at once, without running atexit handlers or static destructors.
#[cfg(target_os = "android")]
pub(crate) fn exit_now() -> ! {
    extern "C" {
        fn _exit(status: core::ffi::c_int) -> !;
    }
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { _exit(0) }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_android_exit_returns_the_companion_lease_first_and_waits_a_bounded_time() {
        let lib = include_str!("lib.rs");
        let exit = lib
            .split("if matches!(event, tauri::RunEvent::Exit) {")
            .nth(1)
            .expect("lib.rs ends the Android process on RunEvent::Exit");
        let returned = exit
            .find("return_companion_lease_before_exit(app);")
            .expect("the exit returns the lease");
        let ended = exit
            .find("process_exit::exit_now();")
            .expect("the exit ends the process");
        assert!(returned < ended, "the lease must go back before the process ends");
        assert!(lib.contains("finished.recv_timeout(EXIT_LEASE_RETURN_WAIT)"));
    }
}
