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
-keepclassmembers class com.aokie.companion.MainActivity {
    public int requestAokieMicrophonePermission(int);
    public int pollAokieMicrophonePermission(int);
    public int requestAokieNotificationPermission(int);
    public int pollAokieNotificationPermission(int);
    public int putAokieSecureValue(java.lang.String, java.lang.String);
    public java.lang.String getAokieSecureValue(java.lang.String);
    public int deleteAokieSecureValue(java.lang.String);
    public int aokieSecureStoreAvailable();
    public java.lang.String aokieRuntimeDiagnostics();
    public int invalidateAokiePushToken();
    public int reconcileAokieOffer(java.lang.String, long, java.lang.String, java.lang.String);
}
