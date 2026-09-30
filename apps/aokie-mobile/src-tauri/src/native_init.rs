//! Native start-up that must run inside a Java-to-native call: the Rust half of
//! `AokieNativeInit.kt`.
//!
//! JNI's `FindClass` uses the class loader of the Java frame that called into native code. Code
//! that Tauri runs from the Android Looper (command handlers, webview callbacks) has no such frame,
//! so a class lookup there goes to the boot class loader and cannot see the app's own classes.
//! libwebrtc finds its Java runtime by name, so initialising it from a command handler aborts the
//! process ("Class not found using the boot class loader", then a JNI abort). The exports below
//! are called by `MainActivity.onCreate` through `AokieNativeInit`, in a frame whose class loader
//! is the app's, before Tauri starts the Rust runtime.
//!
//! Each export's name is the JNI mangling of its Kotlin declaration, so a rename on either side
//! silently turns into an `UnsatisfiedLinkError`; the tests at the bottom pin both spellings.

/// `object AokieNativeInit { @JvmStatic external fun nativeInitWebRtc(context: Context): Boolean }`
/// in package `com.aokie.companion`.
pub const JNI_INIT_WEBRTC: &str = "Java_com_aokie_companion_AokieNativeInit_nativeInitWebRtc";

#[cfg(target_os = "android")]
mod android {
    use std::sync::atomic::{AtomicBool, Ordering};

    use jni::objects::{JClass, JObject};
    use jni::sys::jboolean;
    use jni::JNIEnv;

    static WEBRTC_READY: AtomicBool = AtomicBool::new(false);

    pub fn webrtc_ready() -> bool {
        WEBRTC_READY.load(Ordering::Acquire)
    }

    /// Hands libwebrtc the JVM and the application Context. Returns false when it cannot; the
    /// media features then report themselves unavailable instead of the process dying.
    #[no_mangle]
    #[allow(non_snake_case)]
    pub extern "system" fn Java_com_aokie_companion_AokieNativeInit_nativeInitWebRtc(
        env: JNIEnv,
        _class: JClass,
        context: JObject,
    ) -> jboolean {
        let ready = env
            .get_java_vm()
            .map(|vm| libwebrtc::android::initialize_android_context(&vm, &context))
            .unwrap_or(false);
        WEBRTC_READY.store(ready, Ordering::Release);
        jboolean::from(ready)
    }
}

/// True once libwebrtc's Java side is initialised (Android only: nothing else has one).
#[cfg(target_os = "android")]
pub use android::webrtc_ready;

#[cfg(test)]
mod tests {
    use super::*;

    const KOTLIN: &str = include_str!(
        "../gen/android/app/src/main/java/com/aokie/companion/AokieNativeInit.kt"
    );
    const MAIN_ACTIVITY: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/MainActivity.kt");
    const PROGUARD: &str = include_str!("../gen/android/app/proguard-rules.pro");
    const MEDIA: &str = include_str!("media.rs");
    const SOURCE: &str = include_str!("native_init.rs");

    #[test]
    fn the_rust_export_and_the_kotlin_declaration_name_the_same_method() {
        assert!(KOTLIN.contains("package com.aokie.companion"));
        assert!(KOTLIN.contains("object AokieNativeInit"));
        assert!(KOTLIN.contains("external fun nativeInitWebRtc(context: Context): Boolean"));
        assert_eq!(
            JNI_INIT_WEBRTC,
            "Java_com_aokie_companion_AokieNativeInit_nativeInitWebRtc"
        );
        assert!(
            SOURCE.contains(&format!("pub extern \"system\" fn {JNI_INIT_WEBRTC}")),
            "the exported function must carry the JNI name"
        );
    }

    #[test]
    fn main_activity_initialises_native_code_before_tauri_starts_the_runtime() {
        let init = MAIN_ACTIVITY
            .find("AokieNativeInit.initialize(")
            .expect("MainActivity must call AokieNativeInit.initialize");
        let tauri = MAIN_ACTIVITY
            .find("super.onCreate(")
            .expect("MainActivity calls super.onCreate");
        assert!(
            init < tauri,
            "initialisation must precede super.onCreate, which starts the Rust runtime"
        );
    }

    #[test]
    fn media_no_longer_initialises_libwebrtc_from_the_looper() {
        // The old path ran `initialize_android_context` inside a webview `exec` closure, which
        // is where the boot class loader abort came from. Only the Java-frame export may call it.
        assert_eq!(
            MEDIA.matches("initialize_android_context").count(),
            0,
            "media.rs must not call libwebrtc's Android initialisation itself"
        );
        assert!(MEDIA.contains("native_init::webrtc_ready"));
    }

    #[test]
    fn r8_keeps_the_webrtc_java_runtime_that_native_code_finds_by_name() {
        for rule in [
            "-keep class livekit.org.webrtc.** { *; }",
            "-keep class livekit.org.jni_zero.** { *; }",
            "-dontwarn livekit.org.jni_zero.JniZeroJni",
        ] {
            assert!(PROGUARD.contains(rule), "proguard-rules.pro lacks `{rule}`");
        }
    }
}
