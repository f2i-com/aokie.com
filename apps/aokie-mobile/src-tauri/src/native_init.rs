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
//! # HTTPS
//!
//! `reqwest`'s `rustls` feature verifies server certificates with `rustls-platform-verifier`. On
//! Android that verifier asks the system trust store through JNI, using a small Java component
//! (`org.rustls.platformverifier.CertificateVerifier`, shipped inside the
//! `rustls-platform-verifier-android` crate) and the application Context. Until it has been given
//! that Context it panics on the first TLS handshake ("Expect rustls-platform-verifier to be
//! initialized"), so every HTTPS call the app makes fails on a device. Three pieces make it work,
//! and all three are needed:
//!
//! 1. Gradle: the crate's bundled Maven repository and the `rustls:rustls-platform-verifier`
//!    dependency (`gen/android/app/build.gradle.kts`).
//! 2. R8: the rule `-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }`
//!    (`gen/android/app/proguard-rules.pro`), because R8 cannot see the JNI use and would remove or
//!    rename the component in a minified build.
//! 3. The export below, called before the Rust runtime starts, so the verifier is ready before any
//!    code here can open a connection.
//!
//! The verifier crate uses `jni` 0.22 while Tauri, wry and libwebrtc use 0.21; the two versions
//! coexist under different crate names, and only the verifier export uses 0.22 (`jni22`).
//!
//! Each export's name is the JNI mangling of its Kotlin declaration, so a rename on either side
//! silently turns into an `UnsatisfiedLinkError`; the tests at the bottom pin both spellings.

/// `object AokieNativeInit { @JvmStatic external fun nativeInitWebRtc(context: Context): Boolean }`
/// in package `com.aokie.companion`.
pub const JNI_INIT_WEBRTC: &str = "Java_com_aokie_companion_AokieNativeInit_nativeInitWebRtc";

/// `object AokieNativeInit { @JvmStatic external fun nativeInitPlatformVerifier(context: Context): Boolean }`
/// in package `com.aokie.companion`.
pub const JNI_INIT_PLATFORM_VERIFIER: &str =
    "Java_com_aokie_companion_AokieNativeInit_nativeInitPlatformVerifier";

#[cfg(target_os = "android")]
mod android {
    use std::sync::atomic::{AtomicBool, Ordering};

    use jni::objects::{JClass, JObject};
    use jni::sys::jboolean;
    use jni::JNIEnv;

    static WEBRTC_READY: AtomicBool = AtomicBool::new(false);
    static VERIFIER_READY: AtomicBool = AtomicBool::new(false);

    pub fn webrtc_ready() -> bool {
        WEBRTC_READY.load(Ordering::Acquire)
    }

    pub fn verifier_ready() -> bool {
        VERIFIER_READY.load(Ordering::Acquire)
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

    /// Gives `rustls-platform-verifier` the JVM, the application Context and its class loader.
    /// Returns false (the error is logged) when it cannot; `ensure_https_ready` then keeps refusing
    /// to build HTTPS clients instead of letting the first handshake panic.
    #[no_mangle]
    #[allow(non_snake_case)]
    pub extern "system" fn Java_com_aokie_companion_AokieNativeInit_nativeInitPlatformVerifier<
        'caller,
    >(
        mut env: jni22::EnvUnowned<'caller>,
        _class: jni22::objects::JClass<'caller>,
        context: jni22::objects::JObject<'caller>,
    ) -> jni22::sys::jboolean {
        env.with_env(|env| -> jni22::errors::Result<jni22::sys::jboolean> {
            rustls_platform_verifier::android::init_with_env(env, context)?;
            VERIFIER_READY.store(true, Ordering::Release);
            Ok(true)
        })
        .resolve::<jni22::errors::LogErrorAndDefault>()
    }
}

/// True once libwebrtc's Java side is initialised (Android only: nothing else has one).
#[cfg(target_os = "android")]
pub use android::webrtc_ready;

/// Ok when an HTTPS client may be built. Off Android the platform verifier needs no set-up. On
/// Android this fails closed, with a readable error, while the verifier has not been initialised.
pub fn ensure_https_ready() -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        if !android::verifier_ready() {
            return Err("the Android certificate verifier is not initialised".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KOTLIN: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/AokieNativeInit.kt");
    const MAIN_ACTIVITY: &str =
        include_str!("../gen/android/app/src/main/java/com/aokie/companion/MainActivity.kt");
    const PROGUARD: &str = include_str!("../gen/android/app/proguard-rules.pro");
    const GRADLE: &str = include_str!("../gen/android/app/build.gradle.kts");
    const MEDIA: &str = include_str!("media.rs");
    const SOURCE: &str = include_str!("native_init.rs");

    #[test]
    fn off_android_https_clients_need_no_setup() {
        if cfg!(not(target_os = "android")) {
            assert_eq!(ensure_https_ready(), Ok(()));
        }
    }

    #[test]
    fn the_rust_exports_and_the_kotlin_declarations_name_the_same_methods() {
        assert!(KOTLIN.contains("package com.aokie.companion"));
        assert!(KOTLIN.contains("object AokieNativeInit"));
        for (symbol, declaration) in [
            (
                JNI_INIT_WEBRTC,
                "external fun nativeInitWebRtc(context: Context): Boolean",
            ),
            (
                JNI_INIT_PLATFORM_VERIFIER,
                "external fun nativeInitPlatformVerifier(context: Context): Boolean",
            ),
        ] {
            assert!(KOTLIN.contains(declaration), "Kotlin lacks `{declaration}`");
            // JNI mangles `package.Class.method` into `Java_<package>_<Class>_<method>`.
            let method = declaration
                .strip_prefix("external fun ")
                .and_then(|rest| rest.split('(').next())
                .expect("declaration has a name");
            assert_eq!(
                symbol,
                format!("Java_com_aokie_companion_AokieNativeInit_{method}")
            );
            assert!(
                SOURCE.contains(&format!("pub extern \"system\" fn {symbol}")),
                "the export `{symbol}` must exist under its JNI name"
            );
        }
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
    fn r8_keeps_the_java_runtimes_that_native_code_finds_by_name() {
        for rule in [
            "-keep class livekit.org.webrtc.** { *; }",
            "-keep class livekit.org.jni_zero.** { *; }",
            "-dontwarn livekit.org.jni_zero.JniZeroJni",
            "-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }",
        ] {
            assert!(PROGUARD.contains(rule), "proguard-rules.pro lacks `{rule}`");
        }
    }

    #[test]
    fn gradle_pulls_in_the_verifier_component_and_checks_it_survives_r8() {
        assert!(GRADLE.contains("implementation(\"rustls:rustls-platform-verifier:"));
        assert!(GRADLE.contains("includeGroup(\"rustls\")"));
        assert!(
            GRADLE.contains("\"org.rustls.platformverifier.CertificateVerifier\""),
            "the post-R8 check must list the verifier component"
        );
    }

    #[test]
    fn every_https_client_checks_the_verifier_before_it_is_built() {
        for (file, source) in [
            ("managed_auth.rs", include_str!("managed_auth.rs")),
            ("discovery.rs", include_str!("discovery.rs")),
            ("companion_relay.rs", include_str!("companion_relay.rs")),
        ] {
            let check = source
                .find("native_init::ensure_https_ready()")
                .unwrap_or_else(|| panic!("{file} never calls ensure_https_ready"));
            let build = source
                .find("Client::builder()")
                .unwrap_or_else(|| panic!("{file} builds no client"));
            assert!(
                check < build,
                "{file} must check the verifier before its first Client::builder()"
            );
        }
    }
}
