package com.aokie.companion

import android.Manifest
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.RingtoneManager
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.Person
import androidx.core.content.ContextCompat

internal object AokieCallNotifications {
  // Channel settings are immutable after first creation. v2 deliberately
  // creates a fresh channel so existing installs receive call-ringtone usage
  // rather than retaining the original generic notification sound.
  const val CALL_CHANNEL = "aokie_voice_offers_v2"
  const val INFORMATION_CHANNEL = "aokie_information"

  fun createChannels(context: Context) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = context.getSystemService(NotificationManager::class.java)
    manager.createNotificationChannel(
      NotificationChannel(
        CALL_CHANNEL,
        "Aokie voice offers",
        NotificationManager.IMPORTANCE_HIGH,
      ).apply {
        description = "Genuine, expiring Aokie voice takeover offers"
        lockscreenVisibility = Notification.VISIBILITY_PRIVATE
        setShowBadge(false)
        enableVibration(true)
        setSound(
          RingtoneManager.getDefaultUri(RingtoneManager.TYPE_RINGTONE),
          AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_NOTIFICATION_RINGTONE)
            .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
            .build(),
        )
      },
    )
    manager.createNotificationChannel(
      NotificationChannel(
        INFORMATION_CHANNEL,
        "Aokie updates",
        NotificationManager.IMPORTANCE_DEFAULT,
      ).apply {
        description = "Informational Aokie call and assistance updates"
        lockscreenVisibility = Notification.VISIBILITY_PRIVATE
      },
    )
  }

  fun notificationsAllowed(context: Context): Boolean {
    if (!notificationPermissionGranted(context)) return false
    return channelAllowed(context, CALL_CHANNEL)
  }

  private fun informationNotificationsAllowed(context: Context): Boolean {
    if (!notificationPermissionGranted(context)) return false
    return channelAllowed(context, INFORMATION_CHANNEL)
  }

  private fun notificationPermissionGranted(context: Context): Boolean {
    if (!NotificationManagerCompat.from(context).areNotificationsEnabled()) return false
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
      ContextCompat.checkSelfPermission(context, Manifest.permission.POST_NOTIFICATIONS) !=
      PackageManager.PERMISSION_GRANTED
    ) return false
    return true
  }

  private fun channelAllowed(context: Context, channelId: String): Boolean {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      val manager = context.getSystemService(NotificationManager::class.java)
      if (manager.getNotificationChannel(channelId)?.importance == NotificationManager.IMPORTANCE_NONE) {
        return false
      }
    }
    return true
  }

  fun incoming(context: Context, offer: AokieVoiceOffer): Notification {
    val transfer = offer.acceptedTransferRequestId != null
    val caller = Person.Builder()
      .setName(if (transfer) "Aokie transfer" else "Aokie caller")
      .setImportant(true)
      .build()
    val content = PendingIntent.getActivity(
      context,
      requestCode(offer.offerId, 1),
      Intent(context, MainActivity::class.java)
        .setAction(if (transfer) MainActivity.ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE else Intent.ACTION_MAIN)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val decline = serviceIntent(context, offer.offerId, AokieIncomingCallService.ACTION_DECLINE, 2)
    val answer = serviceIntent(context, offer.offerId, AokieIncomingCallService.ACTION_ANSWER, 3)
    val builder = NotificationCompat.Builder(context, CALL_CHANNEL)
      .setSmallIcon(android.R.drawable.sym_call_incoming)
      .setContentTitle(if (transfer) "Aokie is transferring a caller" else "Aokie voice offer")
      .setContentText(if (transfer) "Answer to accept the live caller" else "Open securely to join the live caller")
      .setContentIntent(content)
      .setCategory(NotificationCompat.CATEGORY_CALL)
      .setPriority(NotificationCompat.PRIORITY_MAX)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .setOngoing(true)
      .setTimeoutAfter((offer.expiresAt * 1000 - System.currentTimeMillis()).coerceAtLeast(1))
      .setStyle(NotificationCompat.CallStyle.forIncomingCall(caller, decline, answer))
      .addPerson(caller)
    val fullScreenAllowed = Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE ||
      context.getSystemService(NotificationManager::class.java).canUseFullScreenIntent()
    if (fullScreenAllowed) {
      builder.setFullScreenIntent(content, true)
    } else {
      // CallStyle remains a high-priority heads-up/lock-screen surface even
      // when the user has disabled full-screen call intents.
      AokieOfferStore.recordDiagnostic(context, "full_screen_call_intent_not_allowed")
    }
    return builder.build()
  }

  fun ongoing(context: Context, offer: AokieVoiceOffer, authoritative: Boolean): Notification {
    val caller = Person.Builder().setName("Aokie caller").setImportant(true).build()
    val content = PendingIntent.getActivity(
      context,
      requestCode(offer.offerId, 4),
      Intent(context, MainActivity::class.java)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val hangup = serviceIntent(
      context,
      offer.offerId,
      if (authoritative) {
        AokieIncomingCallService.ACTION_HANG_UP
      } else {
        AokieIncomingCallService.ACTION_DECLINE
      },
      5,
    )
    return NotificationCompat.Builder(context, CALL_CHANNEL)
      .setSmallIcon(android.R.drawable.sym_call_incoming)
      .setContentTitle(if (authoritative) "Aokie Companion call" else "Securing Aokie call")
      .setContentText(if (authoritative) "Companion audio is controlled by the live lease" else "Waiting for authoritative call state")
      .setContentIntent(content)
      .setCategory(NotificationCompat.CATEGORY_CALL)
      .setPriority(NotificationCompat.PRIORITY_MAX)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .setOngoing(true)
      .setStyle(NotificationCompat.CallStyle.forOngoingCall(caller, hangup))
      .addPerson(caller)
      .build()
  }

  fun informational(context: Context, eventId: String, title: String, body: String) {
    if (!informationNotificationsAllowed(context)) {
      AokieOfferStore.recordDiagnostic(context, "informational_notification_permission_denied")
      return
    }
    val content = PendingIntent.getActivity(
      context,
      requestCode(eventId, 6),
      Intent(context, MainActivity::class.java)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val notification = NotificationCompat.Builder(context, INFORMATION_CHANNEL)
      .setSmallIcon(android.R.drawable.ic_dialog_info)
      .setContentTitle(title)
      .setContentText(body)
      .setContentIntent(content)
      .setAutoCancel(true)
      .setCategory(NotificationCompat.CATEGORY_STATUS)
      .setPriority(NotificationCompat.PRIORITY_DEFAULT)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .build()
    NotificationManagerCompat.from(context).notify(requestCode(eventId, 7), notification)
  }

  fun assistanceOffer(context: Context, eventId: String, expiresAt: Long) {
    if (!informationNotificationsAllowed(context)) {
      AokieOfferStore.recordDiagnostic(context, "assistance_offer_notification_permission_denied")
      return
    }
    val content = PendingIntent.getActivity(
      context,
      requestCode(eventId, 8),
      Intent(context, MainActivity::class.java)
        .setAction(MainActivity.ACTION_REFRESH_AUTHORITATIVE_ASSISTANCE)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    val notification = NotificationCompat.Builder(context, INFORMATION_CHANNEL)
      .setSmallIcon(android.R.drawable.ic_dialog_info)
      .setContentTitle("Aokie needs your help")
      .setContentText("Open Companion to securely fetch the current request")
      .setContentIntent(content)
      .setAutoCancel(true)
      .setCategory(NotificationCompat.CATEGORY_REMINDER)
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
      .setTimeoutAfter((expiresAt * 1000 - System.currentTimeMillis()).coerceAtLeast(1))
      .build()
    NotificationManagerCompat.from(context).notify(requestCode(eventId, 9), notification)
  }

  fun callNotificationId(offerId: String): Int = requestCode(offerId, 41)

  fun cancelCall(context: Context, offerId: String) {
    NotificationManagerCompat.from(context).cancel(callNotificationId(offerId))
  }

  private fun serviceIntent(context: Context, offerId: String, action: String, salt: Int): PendingIntent =
    PendingIntent.getService(
      context,
      requestCode(offerId, salt),
      Intent(context, AokieIncomingCallService::class.java)
        .setAction(action)
        .putExtra(AokieIncomingCallService.EXTRA_OFFER_ID, offerId),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )

  private fun requestCode(value: String, salt: Int): Int =
    (31 * value.hashCode() + salt).and(0x7fffffff)
}
