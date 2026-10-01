import groovy.json.JsonSlurper
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
// trust store, using a small Java component that the `rustls-platform-verifier-android` crate ships as a
// local Maven repository. Cargo.lock pins the crate, so the component's version follows the Rust side.
// `cargo build` normally has unpacked the crate in the Cargo registry before Gradle asks; when it has
// not, `cargo metadata` fetches it and says where it is. Nothing is downloaded from anywhere else.
fun findRustlsPlatformVerifier(): Pair<File, String> {
    val crate = "rustls-platform-verifier-android"
    val workspaceRoot = File(rootDir, "../../../../..").canonicalFile
    val lockedVersion = File(workspaceRoot, "Cargo.lock").takeIf { it.isFile }?.readText()?.let {
        Regex("name = \"$crate\"\\s+version = \"([^\"]+)\"").find(it)?.groupValues?.get(1)
    }
    val cargoHome = System.getenv("CARGO_HOME")?.let(::File) ?: File(System.getProperty("user.home"), ".cargo")
    if (lockedVersion != null) {
        File(cargoHome, "registry/src").listFiles()
            ?.map { File(it, "$crate-$lockedVersion/maven") }
            ?.firstOrNull { it.isDirectory }
            ?.let { return it to lockedVersion }
    }
    val metadata = providers.exec {
        workingDir = rootDir
        commandLine(
            "cargo", "metadata", "--format-version", "1", "--locked",
            "--filter-platform", "x86_64-linux-android",
            "--manifest-path", File(rootDir, "../../Cargo.toml").canonicalPath,
        )
    }.standardOutput.asText.get()
    @Suppress("UNCHECKED_CAST")
    val packages = (JsonSlurper().parseText(metadata) as Map<String, Any?>)["packages"] as List<Map<String, Any?>>
    val found = packages.first { it["name"] == crate }
    return File(File(found["manifest_path"] as String).parentFile, "maven") to (found["version"] as String)
}

val rustlsPlatformVerifier = findRustlsPlatformVerifier()

repositories {
    maven {
        url = uri(rustlsPlatformVerifier.first)
        metadataSources.artifact()
        content { includeGroup("rustls") }
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
            }
        }
    }
}
dependencies {
    // webrtc-sys writes the matching prefixed Java runtime beside each Cargo
    // Android target. The Rust Gradle task copies that architecture-neutral
    // jar here before dex/package tasks execute.
    implementation(files("libs/libwebrtc.jar"))
    // The certificate verifier's Java component (see findRustlsPlatformVerifier above).
    implementation("rustls:rustls-platform-verifier:${rustlsPlatformVerifier.second}")
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
