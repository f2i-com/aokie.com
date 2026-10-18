import java.util.Properties

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("rust")
}

val tauriProperties = Properties().apply {
    val propFile = file("tauri.properties")
    if (propFile.exists()) {
        propFile.inputStream().use { load(it) }
    }
}

val aokieGoogleServicesFile = file("google-services.json")
if (aokieGoogleServicesFile.exists()) {
    apply(plugin = "com.google.gms.google-services")
}

android {
    compileSdk = 36
    namespace = "com.aokie.companion"
    defaultConfig {
        manifestPlaceholders["usesCleartextTraffic"] = "false"
        applicationId = "com.aokie.companion"
        minSdk = 24
        targetSdk = 36
        versionCode = tauriProperties.getProperty("tauri.android.versionCode", "1").toInt()
        versionName = tauriProperties.getProperty("tauri.android.versionName", "1.0")
        buildConfigField(
            "boolean",
            "AOKIE_FCM_CONFIG_PRESENT",
            aokieGoogleServicesFile.exists().toString(),
        )
    }
    buildTypes {
        getByName("debug") {
            manifestPlaceholders["usesCleartextTraffic"] = "true"
            isDebuggable = true
            isJniDebuggable = true
            isMinifyEnabled = false
            packaging {
                jniLibs.keepDebugSymbols.add("*/arm64-v8a/*.so")
                jniLibs.keepDebugSymbols.add("*/armeabi-v7a/*.so")
                jniLibs.keepDebugSymbols.add("*/x86/*.so")
                jniLibs.keepDebugSymbols.add("*/x86_64/*.so")
            }
        }
        getByName("release") {
            isMinifyEnabled = true
            proguardFiles(
                *fileTree(".") { include("**/*.pro") }
                    .plus(getDefaultProguardFile("proguard-android-optimize.txt"))
                    .toList().toTypedArray()
            )
        }
    }
    kotlinOptions {
        jvmTarget = "1.8"
    }
    buildFeatures {
        buildConfig = true
    }
}

rust {
    rootDirRel = "../../../"
}

// rustls-platform-verifier: reqwest's `rustls` feature verifies server certificates through the Android
// trust store, using a small Java component, org.rustls:rustls-platform-verifier. Since the
// `rustls-platform-verifier-android` crate moved to 0.2.0 the crate no longer carries that component: it
// is published as a Maven archive on GitHub (the repository below, which is what the crate's own Gradle
// instructions give), so an Android build now downloads it from there. Cargo.lock pins the crate, so the
// component's version follows the Rust side; the repository is limited to the org.rustls group so that
// nothing else is looked up there.
val rustlsPlatformVerifierVersion: String = run {
    val crate = "rustls-platform-verifier-android"
    val lockFile = File(rootDir, "../../../../../Cargo.lock").canonicalFile
    if (!lockFile.isFile) throw GradleException("Cargo.lock not found at $lockFile")
    Regex("name = \"$crate\"\\s+version = \"([^\"]+)\"").find(lockFile.readText())?.groupValues?.get(1)
        ?: throw GradleException("$crate not found in $lockFile")
}

repositories {
    maven {
        url = uri("https://github.com/rustls/rustls-platform-verifier/raw/maven-archive/android-release-support/maven/")
        content { includeGroup("org.rustls") }
    }
}

// R8 cannot see JNI use, so the classes that native code looks up by name are kept by rules in
// proguard-rules.pro. This check runs right after every R8 pass and fails the build if R8's own mapping
// shows one of them removed or renamed, which is what a missing or mistyped rule looks like.
val jniKeptClasses = listOf(
    "livekit.org.webrtc.ContextUtils",
    "livekit.org.jni_zero.JniZero",
    "org.rustls.platformverifier.CertificateVerifier",
)
// Rust also calls MainActivity's bridge methods by name. Their names come from the keepclassmembers block in
// proguard-rules.pro (a unit test in android_runtime.rs checks that block lists every name Rust calls), and the
// check below fails if R8's mapping shows one renamed or its usage report shows one removed.
val jniBridgeClass = "com.aokie.companion.MainActivity"
val jniBridgeMethods: List<String> = run {
    val rules = file("proguard-rules.pro").readText()
    val block = Regex("-keepclassmembers class " + Regex.escape(jniBridgeClass) + " \\{([^}]*)\\}").find(rules)
        ?.groupValues?.get(1)
        ?: throw GradleException("proguard-rules.pro has no keepclassmembers block for $jniBridgeClass")
    Regex("^\\s*public\\s+\\S+\\s+([A-Za-z_][A-Za-z0-9_]*)\\(", RegexOption.MULTILINE)
        .findAll(block).map { it.groupValues[1] }.toList()
        .ifEmpty { throw GradleException("proguard-rules.pro names no $jniBridgeClass bridge methods") }
}
val r8MappingDir = layout.buildDirectory.dir("outputs/mapping")
tasks.configureEach {
    if (name.startsWith("minify") && name.endsWith("WithR8")) {
        doLast {
            val mappings = r8MappingDir.get().asFile.walkTopDown().filter { it.name == "mapping.txt" }.toList()
            if (mappings.isEmpty()) throw GradleException("R8 wrote no mapping.txt under ${r8MappingDir.get().asFile}")
            for (mapping in mappings) {
                val text = mapping.readText()
                for (name in jniKeptClasses) {
                    val kept = Regex("^" + Regex.escape(name) + " -> " + Regex.escape(name) + ":\\r?$", RegexOption.MULTILINE)
                    if (!kept.containsMatchIn(text)) {
                        throw GradleException("R8 removed or renamed $name (${mapping.path}); native code finds it by name, see proguard-rules.pro")
                    }
                }
                // A renamed member is listed under its class as `<type> <name>(<args>)[:lines] -> <new name>`.
                val bridgeBlock = Regex(
                    "^" + Regex.escape(jniBridgeClass) + " -> [^\\r\\n]*:\\r?$((?:\\r?\\n[ \\t]+[^\\r\\n]*)*)",
                    RegexOption.MULTILINE,
                ).find(text)?.groupValues?.get(1).orEmpty()
                for (method in jniBridgeMethods) {
                    val renamed = Regex("\\s" + Regex.escape(method) + "\\([^)]*\\)[0-9:]* -> (\\S+)")
                        .findAll(bridgeBlock).any { it.groupValues[1] != method }
                    if (renamed) {
                        throw GradleException("R8 renamed $jniBridgeClass.$method (${mapping.path}); Rust calls it by name, see proguard-rules.pro")
                    }
                }
                // R8's usage report, where it writes one, lists removed members indented under their class.
                val usage = File(mapping.parentFile, "usage.txt")
                if (usage.isFile) {
                    var inBridge = false
                    for (line in usage.readLines()) {
                        if (line.isNotEmpty() && !line[0].isWhitespace()) {
                            inBridge = line.trimEnd() == "$jniBridgeClass:"
                            continue
                        }
                        val removed = jniBridgeMethods.firstOrNull { inBridge && line.contains(" $it(") }
                        if (removed != null) {
                            throw GradleException("R8 removed $jniBridgeClass.$removed (${usage.path}); Rust calls it by name, see proguard-rules.pro")
                        }
                    }
                }
            }
        }
    }
}
dependencies {
    // webrtc-sys writes the matching prefixed Java runtime beside each Cargo
    // Android target. The Rust Gradle task copies that architecture-neutral
    // jar here before dex/package tasks execute.
    implementation(files("libs/libwebrtc.jar"))
    // The certificate verifier's Java component (see rustlsPlatformVerifierVersion above).
    implementation("org.rustls:rustls-platform-verifier:$rustlsPlatformVerifierVersion")
    implementation("androidx.webkit:webkit:1.14.0")
    implementation("androidx.appcompat:appcompat:1.7.1")
    implementation("androidx.activity:activity-ktx:1.10.1")
    implementation("com.google.android.material:material:1.12.0")
    implementation("androidx.lifecycle:lifecycle-process:2.10.0")
    implementation("androidx.core:core-telecom:1.0.0")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.10.2")
    implementation(platform("com.google.firebase:firebase-bom:34.15.0"))
    implementation("com.google.firebase:firebase-messaging")
    testImplementation("junit:junit:4.13.2")
    androidTestImplementation("androidx.test.ext:junit:1.1.4")
    androidTestImplementation("androidx.test.espresso:espresso-core:3.5.0")
}

apply(from = "tauri.build.gradle.kts")
