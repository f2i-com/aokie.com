import java.io.File
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import org.apache.tools.ant.taskdefs.condition.Os
import org.gradle.api.DefaultTask
import org.gradle.api.GradleException
import org.gradle.api.logging.LogLevel
import org.gradle.api.tasks.Input
import org.gradle.api.tasks.TaskAction

open class BuildTask : DefaultTask() {
    companion object {
        private val webRtcJarCopyLock = Any()
    }
    @Input
    var rootDirRel: String? = null
    @Input
    var target: String? = null
    @Input
    var release: Boolean? = null

    @TaskAction
    fun assemble() {
        val executable = """npm""";
        try {
            runTauriCli(executable)
        } catch (e: Exception) {
            if (Os.isFamily(Os.FAMILY_WINDOWS)) {
                // Try different Windows-specific extensions
                val fallbacks = listOf(
                    "$executable.exe",
                    "$executable.cmd",
                    "$executable.bat",
                )
                var lastException: Exception = e
                for (fallback in fallbacks) {
                    try {
                        runTauriCli(fallback)
                        return
                    } catch (fallbackException: Exception) {
                        lastException = fallbackException
                    }
                }
                throw lastException
            } else {
                throw e;
            }
        }
    }

    fun runTauriCli(executable: String) {
        val rootDirRel = rootDirRel ?: throw GradleException("rootDirRel cannot be null")
        val target = target ?: throw GradleException("target cannot be null")
        val release = release ?: throw GradleException("release cannot be null")
        val args = listOf("run", "--", "tauri", "android", "android-studio-script");

        project.exec {
            workingDir(File(project.projectDir, rootDirRel))
            executable(executable)
            args(args)
            if (project.logger.isEnabled(LogLevel.DEBUG)) {
                args("-vv")
            } else if (project.logger.isEnabled(LogLevel.INFO)) {
                args("-v")
            }
            if (release) {
                args("--release")
            }
            args(listOf("--target", target))
        }.assertNormalExitValue()

        copyWebRtcRuntimeJar(target, release)
    }

    private fun copyWebRtcRuntimeJar(target: String, release: Boolean) {
        val targetTriple = when (target) {
            "aarch64" -> "aarch64-linux-android"
            "armv7" -> "armv7-linux-androideabi"
            "i686" -> "i686-linux-android"
            "x86_64" -> "x86_64-linux-android"
            else -> throw GradleException("unsupported Rust Android target: $target")
        }
        val profile = if (release) "release" else "debug"
        val tauriRoot = File(project.projectDir, rootDirRel!!).canonicalFile
        val workspaceRoot = tauriRoot.parentFile.parentFile.parentFile
        val targetDir = cargoTargetDir(tauriRoot, workspaceRoot)
        val source = findWebRtcRuntimeJar(target, targetTriple, profile, targetDir)
            ?: throw GradleException(
                "libwebrtc Java runtime (libwebrtc.jar) for $targetTriple was not found under ${targetDir.absolutePath}",
            )
        val destination = File(project.projectDir, "libs/libwebrtc.jar")
        synchronized(webRtcJarCopyLock) {
            destination.parentFile.mkdirs()
            Files.copy(source.toPath(), destination.toPath(), StandardCopyOption.REPLACE_EXISTING)
        }
    }

    /** Cargo's target directory: `CARGO_TARGET_DIR` when set (a relative one counts from where Cargo runs), else the workspace's. */
    private fun cargoTargetDir(tauriRoot: File, workspaceRoot: File): File {
        val fromEnv = System.getenv("CARGO_TARGET_DIR")?.takeIf { it.isNotBlank() } ?: return File(workspaceRoot, "target")
        val dir = File(fromEnv)
        return (if (dir.isAbsolute) dir else File(tauriRoot, fromEnv)).canonicalFile
    }

    /**
     * Finds the libwebrtc Java runtime that goes with the native build for [targetTriple].
     *
     * webrtc-sys's build script copies the jar to `<webrtc-sys directory>/../target/<triple>/<profile>/`. That
     * directory only exists when webrtc-sys sits in a Cargo workspace of its own (LiveKit's repository); a crate
     * downloaded from the registry has none, the copy fails ("Failed to copy libwebrtc.jar" in the build script's
     * output) and nothing reaches this workspace's target directory. The jar is always in the prebuilt libwebrtc
     * that webrtc-sys-build downloaded, which lives in the `scratch` crate's output directory of the host build
     * profile: `<target>/<profile>/build/scratch-<hash>/out/livekit_webrtc/livekit/android-<arch>-release-<tag>/
     * android-<arch>-release/libwebrtc.jar`. The Java classes are the same for every architecture, but the
     * architecture's own copy is taken, and the newest when a libwebrtc upgrade left older downloads behind.
     */
    private fun findWebRtcRuntimeJar(target: String, targetTriple: String, profile: String, targetDir: File): File? {
        // Where webrtc-sys leaves it when it is built inside LiveKit's own workspace layout.
        val besideTheBuild = File(targetDir, "$targetTriple/$profile/libwebrtc.jar")
        if (besideTheBuild.isFile) return besideTheBuild

        // A libwebrtc build the developer pointed webrtc-sys-build at.
        System.getenv("LK_CUSTOM_WEBRTC")?.takeIf { it.isNotBlank() }?.let {
            val custom = File(it, "libwebrtc.jar")
            if (custom.isFile) return custom
        }

        val webRtcArch = when (target) {
            "aarch64" -> "arm64"
            "armv7" -> "arm"
            "i686" -> "x86"
            "x86_64" -> "x64"
            else -> throw GradleException("unsupported Rust Android target: $target")
        }
        val prebuilt = "android-$webRtcArch-release"
        return targetDir.listFiles { dir -> dir.isDirectory }.orEmpty()
            .flatMap { File(it, "build").listFiles { dir -> dir.isDirectory && dir.name.startsWith("scratch-") }.orEmpty().toList() }
            .map { File(it, "out/livekit_webrtc/livekit") }
            .flatMap { it.listFiles { dir -> dir.isDirectory && dir.name.startsWith("$prebuilt-") }.orEmpty().toList() }
            .map { File(it, "$prebuilt/libwebrtc.jar") }
            .filter { it.isFile }
            .maxByOrNull { it.lastModified() }
    }
}
