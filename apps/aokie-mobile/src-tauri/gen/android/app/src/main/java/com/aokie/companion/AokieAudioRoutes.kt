package com.aokie.companion

import android.content.Context
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.media.ToneGenerator
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/**
 * Android owns communication-device routing. Rust can request one of the
 * currently enumerated routes, but it never opens Bluetooth or the handset
 * radio directly and never persists a device address.
 */
internal object AokieAudioRoutes {
  private val initialized = AtomicBoolean(false)
  private val sessionActive = AtomicBoolean(false)
  private val revision = AtomicLong(1)

  private val callback = object : AudioDeviceCallback() {
    override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
      revision.incrementAndGet()
    }

    override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
      revision.incrementAndGet()
    }
  }

  fun initialize(context: Context) {
    if (!initialized.compareAndSet(false, true)) return
    val audio = context.applicationContext.getSystemService(AudioManager::class.java)
    audio.registerAudioDeviceCallback(callback, Handler(Looper.getMainLooper()))
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      audio.addOnCommunicationDeviceChangedListener(context.mainExecutor) {
        revision.incrementAndGet()
      }
    }
  }

  fun begin(context: Context): Int = runCatching {
    initialize(context)
    val audio = context.getSystemService(AudioManager::class.java)
    audio.mode = AudioManager.MODE_IN_COMMUNICATION
    sessionActive.set(true)
    revision.incrementAndGet()
    1
  }.getOrElse {
    AokieOfferStore.recordDiagnostic(context, "communication_audio_begin_failed")
    -1
  }

  fun end(context: Context): Int = runCatching {
    val audio = context.getSystemService(AudioManager::class.java)
    sessionActive.set(false)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      audio.clearCommunicationDevice()
    } else {
      @Suppress("DEPRECATION")
      audio.isSpeakerphoneOn = false
      @Suppress("DEPRECATION")
      audio.isBluetoothScoOn = false
      @Suppress("DEPRECATION")
      audio.stopBluetoothSco()
    }
    audio.mode = AudioManager.MODE_NORMAL
    revision.incrementAndGet()
    1
  }.getOrElse {
    AokieOfferStore.recordDiagnostic(context, "communication_audio_end_failed")
    -1
  }

  fun snapshot(context: Context): String {
    initialize(context)
    val audio = context.getSystemService(AudioManager::class.java)
    val routes = routes(audio)
    val selected = selectedRoute(audio, routes)
    val state = if (sessionActive.get()) "media_active" else "idle"
    return JSONObject()
      .put("schemaVersion", 1)
      .put("revision", revision.get().coerceAtMost(9_007_199_254_740_991L))
      .put("routes", JSONArray().apply {
        routes.forEach { route ->
          put(JSONObject().put("id", route.id).put("kind", route.kind).put("label", route.label))
        }
      })
      .put("selectedId", selected?.id ?: "system_managed")
      .put("canSelect", sessionActive.get() && routes.isNotEmpty())
      .put("state", state)
      .toString()
  }

  fun select(context: Context, routeId: String): String {
    require(routeId.length in 1..200 && routeId.all { it.isLetterOrDigit() || it in "-_.:" })
    require(sessionActive.get()) { "communication session is not active" }
    val audio = context.getSystemService(AudioManager::class.java)
    val routes = routes(audio)
    val selected = routes.singleOrNull { it.id == routeId }
      ?: error("requested communication route is unavailable")
    val changed = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      audio.setCommunicationDevice(selected.device)
    } else {
      selectLegacy(audio, selected.kind)
    }
    check(changed) { "Android rejected the communication route" }
    revision.incrementAndGet()
    return snapshot(context)
  }

  /** Called only for an authoritative native false -> true live-media edge. */
  fun signalLiveTransition(context: Context): Int = runCatching {
    check(sessionActive.get()) { "communication session is not active" }
    val vibrator = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      context.getSystemService(VibratorManager::class.java).defaultVibrator
    } else {
      @Suppress("DEPRECATION")
      context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
    }
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      vibrator.vibrate(VibrationEffect.createOneShot(45, VibrationEffect.DEFAULT_AMPLITUDE))
    } else {
      @Suppress("DEPRECATION")
      vibrator.vibrate(45)
    }
    val tone = ToneGenerator(AudioManager.STREAM_VOICE_CALL, 30)
    tone.startTone(ToneGenerator.TONE_PROP_BEEP, 80)
    Handler(Looper.getMainLooper()).postDelayed({ tone.release() }, 140)
    1
  }.getOrElse {
    AokieOfferStore.recordDiagnostic(context, "live_transition_cue_failed")
    -1
  }

  private data class Route(
    val id: String,
    val kind: String,
    val label: String,
    val device: AudioDeviceInfo,
  )

  private fun routes(audio: AudioManager): List<Route> {
    val devices = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      audio.availableCommunicationDevices
    } else {
      audio.getDevices(AudioManager.GET_DEVICES_OUTPUTS).toList()
    }
    return devices.mapNotNull(::route).distinctBy(Route::id).sortedWith(
      compareBy<Route>({ routePriority(it.kind) }, { it.id }),
    )
  }

  private fun route(device: AudioDeviceInfo): Route? {
    val kind = when (device.type) {
      AudioDeviceInfo.TYPE_BUILTIN_SPEAKER -> "speaker"
      AudioDeviceInfo.TYPE_BUILTIN_EARPIECE -> "earpiece"
      AudioDeviceInfo.TYPE_WIRED_HEADSET,
      AudioDeviceInfo.TYPE_WIRED_HEADPHONES,
      AudioDeviceInfo.TYPE_USB_HEADSET,
      AudioDeviceInfo.TYPE_USB_DEVICE,
      -> "wired"
      AudioDeviceInfo.TYPE_BLUETOOTH_SCO,
      AudioDeviceInfo.TYPE_BLE_HEADSET,
      AudioDeviceInfo.TYPE_BLE_SPEAKER,
      AudioDeviceInfo.TYPE_HEARING_AID,
      -> "bluetooth"
      else -> return null
    }
    val label = when (kind) {
      "speaker" -> "Speaker"
      "earpiece" -> "Earpiece"
      "wired" -> "Wired or USB headset"
      else -> "Bluetooth or hearing device"
    }
    return Route("$kind:${device.id}", kind, label, device)
  }

  private fun selectedRoute(audio: AudioManager, routes: List<Route>): Route? {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
      val selected = audio.communicationDevice ?: return null
      return routes.firstOrNull { it.device.id == selected.id }
    }
    @Suppress("DEPRECATION")
    if (audio.isSpeakerphoneOn) return routes.firstOrNull { it.kind == "speaker" }
    @Suppress("DEPRECATION")
    if (audio.isBluetoothScoOn) return routes.firstOrNull { it.kind == "bluetooth" }
    return routes.firstOrNull { it.kind == "wired" }
      ?: routes.firstOrNull { it.kind == "earpiece" }
      ?: routes.firstOrNull { it.kind == "speaker" }
  }

  @Suppress("DEPRECATION")
  private fun selectLegacy(audio: AudioManager, kind: String): Boolean {
    return when (kind) {
      "speaker" -> {
        audio.stopBluetoothSco()
        audio.isBluetoothScoOn = false
        audio.isSpeakerphoneOn = true
        true
      }
      "bluetooth" -> {
        audio.isSpeakerphoneOn = false
        audio.startBluetoothSco()
        audio.isBluetoothScoOn = true
        true
      }
      "wired", "earpiece" -> {
        audio.stopBluetoothSco()
        audio.isBluetoothScoOn = false
        audio.isSpeakerphoneOn = false
        true
      }
      else -> false
    }
  }

  private fun routePriority(kind: String): Int = when (kind) {
    "earpiece" -> 0
    "speaker" -> 1
    "wired" -> 2
    else -> 3
  }
}
