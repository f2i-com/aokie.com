package com.aokie.companion

import android.Manifest
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat

/**
 * Keeps Companion media working while the app is off screen.
 *
 * In-app Talk and Consult have no system call surface (AokieIncomingCallService runs only for an offer
 * answered from a notification), so nothing told Android a call was live: when the screen timed out
 * mid-takeover, Android silenced the app's microphone and the caller heard nothing. This service runs from
 * the moment native media opens communication audio ([AokieAudioRoutes.begin]) until it closes it
 * ([AokieAudioRoutes.end]), as a foreground service of type `microphone` (once the microphone permission is
 * granted) and `phoneCall`, so the microphone keeps working once the screen is off or the app is in the
 * background. While the audio is on the earpiece it also holds a proximity wake lock, so the screen goes dark
 * against the ear, as in a phone call, and a cheek cannot press Return or Hang up.
 *
 * It is started with a plain `startService` from the app on screen, never `startForegroundService`: if
 * Android refuses the foreground start, the service just stops, with no deadline to miss and no crash.
 */
class AokieCommunicationService : Service() {
  companion object {
    private const val ACTION_START = "com.aokie.companion.action.COMMUNICATION_START"
    private const val ACTION_REFRESH = "com.aokie.companion.action.COMMUNICATION_REFRESH"
    private const val NOTIFICATION_ID = 0x414f4b31 // "AOK1"; call notifications use hashed ids
    private const val WAKE_LOCK_TAG = "aokie:companion-proximity"
    private const val WAKE_LOCK_LIMIT_MS = 4 * 60 * 60 * 1000L
    private const val MICROPHONE_AND_CALL =
      ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE or ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL

    @Volatile
    private var running = false

    /** Called when communication audio begins. Never throws; false when Android refused the start. */
    fun start(context: Context): Boolean = runCatching {
      context.startService(Intent(context, AokieCommunicationService::class.java).setAction(ACTION_START))
      true
    }.getOrElse {
      AokieOfferStore.recordDiagnostic(context, "communication_service_start_failed")
      false
    }

    /** Re-reads the microphone permission and the audio route; does nothing while the service is stopped. */
    fun refresh(context: Context) {
      if (!running) return
      runCatching {
        context.startService(Intent(context, AokieCommunicationService::class.java).setAction(ACTION_REFRESH))
      }
    }

    /** Called when communication audio ends. Never throws. */
    fun stop(context: Context) {
      runCatching { context.stopService(Intent(context, AokieCommunicationService::class.java)) }
    }
  }

  private var proximityLock: PowerManager.WakeLock? = null
  private var foreground = false
  private var microphoneTyped = false

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val action = intent?.action
    if (action != ACTION_START && action != ACTION_REFRESH) {
      if (!foreground) stopSelf()
      return START_NOT_STICKY
    }
    if (!foreground) {
      // A refresh never starts the service: only communication audio beginning does.
      if (action != ACTION_START || !enterForeground()) {
        if (action == ACTION_START) {
          AokieOfferStore.recordDiagnostic(this, "communication_service_foreground_refused")
        }
        running = false
        releaseProximityLock()
        stopSelf()
        return START_NOT_STICKY
      }
      foreground = true
      running = true
    } else {
      addMicrophoneType()
    }
    updateProximityLock()
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    running = false
    foreground = false
    releaseProximityLock()
    super.onDestroy()
  }

  /** Enters the foreground, with the microphone type when it is granted; false when Android refuses. */
  private fun enterForeground(): Boolean {
    AokieCallNotifications.createChannels(this)
    val notification = notification()
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
      return runCatching { startForeground(NOTIFICATION_ID, notification) }.isSuccess
    }
    if (microphoneGranted() && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
      runCatching { startForeground(NOTIFICATION_ID, notification, MICROPHONE_AND_CALL) }.isSuccess
    ) {
      microphoneTyped = true
      return true
    }
    microphoneTyped = false
    return runCatching {
      startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_PHONE_CALL)
    }.isSuccess
  }

  /**
   * Adds the microphone type once the permission arrives after the session began (Talk asks for it only
   * when the lease is armed). Never narrows the type: Android refuses a microphone type while the app is
   * off screen, and a refresh from an audio-device change can come then.
   */
  private fun addMicrophoneType() {
    if (microphoneTyped || !microphoneGranted() || Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
    runCatching { startForeground(NOTIFICATION_ID, notification(), MICROPHONE_AND_CALL) }
      .onSuccess { microphoneTyped = true }
      .onFailure { AokieOfferStore.recordDiagnostic(this, "communication_service_microphone_type_refused") }
  }

  private fun microphoneGranted(): Boolean =
    ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

  private fun notification(): android.app.Notification {
    val content = PendingIntent.getActivity(
      this,
      NOTIFICATION_ID,
      Intent(this, MainActivity::class.java)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    return NotificationCompat.Builder(this, AokieCallNotifications.LIVE_AUDIO_CHANNEL)
      .setSmallIcon(android.R.drawable.sym_call_outgoing)
      .setContentTitle("Aokie Companion audio is on")
      .setContentText("Open Companion to return to the call")
      .setContentIntent(content)
      .setCategory(NotificationCompat.CATEGORY_SERVICE)
      .setPriority(NotificationCompat.PRIORITY_LOW)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .setOngoing(true)
      .setSilent(true)
      .build()
  }

  private fun updateProximityLock() {
    runCatching {
      val power = getSystemService(PowerManager::class.java)
      val wanted = AokieAudioRoutes.earpieceSelected(this) &&
        power.isWakeLockLevelSupported(PowerManager.PROXIMITY_SCREEN_OFF_WAKE_LOCK)
      if (!wanted) {
        releaseProximityLock()
        return@runCatching
      }
      val lock = proximityLock ?: power.newWakeLock(PowerManager.PROXIMITY_SCREEN_OFF_WAKE_LOCK, WAKE_LOCK_TAG)
        .apply { setReferenceCounted(false) }
        .also { proximityLock = it }
      // Bounded, so a lost end can never hold the lock for good.
      if (!lock.isHeld) lock.acquire(WAKE_LOCK_LIMIT_MS)
    }.onFailure { AokieOfferStore.recordDiagnostic(this, "communication_proximity_lock_failed") }
  }

  private fun releaseProximityLock() {
    runCatching { proximityLock?.takeIf { it.isHeld }?.release() }
    proximityLock = null
  }
}
