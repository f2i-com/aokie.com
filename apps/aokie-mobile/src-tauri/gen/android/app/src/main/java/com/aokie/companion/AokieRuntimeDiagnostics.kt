package com.aokie.companion

import android.Manifest
import android.app.NotificationManager
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.PowerManager
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import org.json.JSONObject

internal object AokieRuntimeDiagnostics {
  fun snapshot(context: Context): String {
    AokieCallNotifications.createChannels(context)
    val notificationPermission = permissionState(context, Manifest.permission.POST_NOTIFICATIONS, 33)
    val microphonePermission = permissionState(context, Manifest.permission.RECORD_AUDIO, 1)
    val callChannelEnabled = if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
      true
    } else {
      context.getSystemService(NotificationManager::class.java)
        .getNotificationChannel(AokieCallNotifications.CALL_CHANNEL)
        ?.importance != NotificationManager.IMPORTANCE_NONE
    }
    val notificationsEnabled = NotificationManagerCompat.from(context).areNotificationsEnabled() &&
      notificationPermission != "denied" && callChannelEnabled
    val manageOwnCallsDeclared = requestedPermissionDeclared(context, Manifest.permission.MANAGE_OWN_CALLS)
    val nativeCallUi = Build.VERSION.SDK_INT >= Build.VERSION_CODES.O &&
      manageOwnCallsDeclared && notificationsEnabled
    val fcmConfigured = runCatching { AokiePushRegistration.configured(context) }.getOrDefault(false)
    val fcmTokenPresent = AokiePushRegistration.tokenPresent(context)
    val pendingOffer = AokieOfferStore.current(context, clearExpired = true) != null
    val power = context.getSystemService(PowerManager::class.java)
    val batteryRestricted = !power.isIgnoringBatteryOptimizations(context.packageName)
    val pushState = when {
      !BuildConfig.AOKIE_FCM_CONFIG_PRESENT -> "configuration_required"
      !fcmConfigured -> "initialization_required"
      !fcmTokenPresent -> "token_pending"
      AokiePushRegistration.registrationCurrent(context) -> "registered"
      else -> "endpoint_registration_required"
    }
    return JSONObject()
      .put("secureStorage", AokieSecureStore.isAvailable(context))
      .put("notificationPermission", notificationPermission)
      .put("microphonePermission", microphonePermission)
      .put("notificationsEnabled", notificationsEnabled)
      .put("nativeCallUi", nativeCallUi)
      .put("fcmConfigured", fcmConfigured)
      .put("fcmTokenPresent", fcmTokenPresent)
      .put("pushRegistration", pushState)
      .put("pendingCallOffer", pendingOffer)
      .put("batteryOptimizationsRestricted", batteryRestricted)
      .put("forceStopState", "not_detectable")
      .put(
        "callInfrastructure",
        when {
          Build.VERSION.SDK_INT < Build.VERSION_CODES.O -> "android_8_required"
          !notificationsEnabled -> "notification_permission_required"
          else -> "ready_for_authoritative_offers"
        },
      )
      .put("lastNativeDiagnostic", AokieOfferStore.diagnostic(context) ?: JSONObject.NULL)
      .toString()
  }

  private fun permissionState(context: Context, permission: String, introducedAt: Int): String {
    if (Build.VERSION.SDK_INT < introducedAt) return "not_required"
    return if (ContextCompat.checkSelfPermission(context, permission) == PackageManager.PERMISSION_GRANTED) {
      "granted"
    } else {
      "denied"
    }
  }

  private fun requestedPermissionDeclared(context: Context, permission: String): Boolean = runCatching {
    val info = context.packageManager.getPackageInfo(context.packageName, PackageManager.GET_PERMISSIONS)
    info.requestedPermissions?.contains(permission) == true
  }.getOrDefault(false)
}
