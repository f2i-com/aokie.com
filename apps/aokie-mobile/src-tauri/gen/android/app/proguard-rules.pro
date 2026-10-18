# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile

# Rust calls this narrow native bridge by exact JNI method name. These methods
# are never JavaScript interfaces, but release R8 must not rename or remove
# them. The values stay inside Rust/Kotlin and are not exposed to the WebView.
# A unit test in android_runtime.rs checks every method name Rust calls is
# listed here, and the post-R8 check in build.gradle.kts reads this block.
-keepclassmembers class com.aokie.companion.MainActivity {
    public int requestAokieMicrophonePermission(int);
    public int pollAokieMicrophonePermission(int);
    public int requestAokieNotificationPermission(int);
    public int pollAokieNotificationPermission(int);
    public int openAokieNotificationSettings();
    public int putAokieSecureValue(java.lang.String, java.lang.String);
    public java.lang.String getAokieSecureValue(java.lang.String);
    public int deleteAokieSecureValue(java.lang.String);
    public int aokieSecureStoreAvailable();
    public java.lang.String aokieRuntimeDiagnostics();
    public int invalidateAokiePushToken();
    public java.lang.String takeAokieNativeCallAction();
    public int completeAokieNativeCallAction(java.lang.String, int, java.lang.String);
    public int presentAokieAuthoritativeOffer(java.lang.String);
    public int beginAokieCommunicationAudio();
    public int endAokieCommunicationAudio();
    public java.lang.String aokieAudioRoutes();
    public java.lang.String selectAokieAudioRoute(java.lang.String);
    public int signalAokieLiveTransition();
    public int reconcileAokieOffer(java.lang.String, long, java.lang.String, java.lang.String);
    # A safety net for a bridge method added later without its own line.
    public *** *Aokie*(...);
    public *** aokie*(...);
}

# libwebrtc's Java runtime is reached only from native code, by class name (JNI FindClass and
# GetMethodID), so R8 sees no use and removes it: the minified build then dies the first time WebRTC
# starts. The jar carries no consumer rules of its own. Class names are the repackaged ones.
-keep class livekit.org.webrtc.** { *; }
-keep class livekit.org.jni_zero.** { *; }
# The generated glue class the jar's JniZero refers to is not shipped; nothing calls the method that uses it.
-dontwarn livekit.org.jni_zero.JniZeroJni

# rustls-platform-verifier's Java component is reached only through JNI from the Rust verifier, so R8 sees
# no use and would remove or rename it, and every HTTPS request would fail in the minified build.
-keep, includedescriptorclasses class org.rustls.platformverifier.** { *; }
