package com.airbridge.app.core

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Binder
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import com.airbridge.app.MainActivity
import com.airbridge.app.R
import com.airbridge.app.model.DiscoveryUiState
import com.airbridge.app.model.IncomingPhase
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.app.model.ShareDirection
import com.airbridge.app.model.TransferPhase
import com.airbridge.app.model.TransferUi
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.launch

/**
 * Foreground service of type `connectedDevice`.
 *
 * It keeps discovery and any in-flight transfer alive while the app is in the
 * background, holds the multicast lock mDNS needs, and mirrors the current
 * state into an ongoing notification.
 */
class AirBridgeService : Service() {
    private val serviceScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val binder = LocalBinder()
    private var notificationJob: Job? = null
    private var multicastLock: WifiManager.MulticastLock? = null
    private var lastSignature: String? = null

    lateinit var controller: AirBridgeController
        private set

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
        showForeground(buildNotification(null, null, null))
        acquireMulticastLock()
        controller = AirBridgeController(applicationContext, serviceScope, AppPreferences(this))
        controller.start()
        notificationJob = serviceScope.launch {
            combine(
                controller.discovery,
                controller.transfer,
                controller.incoming,
            ) { discovery, transfer, incoming ->
                Triple(discovery, transfer, incoming)
            }.collect { (discovery, transfer, incoming) ->
                updateNotification(discovery, transfer, incoming)
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        return when (intent?.action) {
            ACTION_STOP -> {
                controller.stopSharing()
                ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
                stopSelf()
                START_NOT_STICKY
            }
            else -> {
                if (::controller.isInitialized) {
                    showForeground(
                        buildNotification(
                            controller.discovery.value,
                            controller.transfer.value,
                            controller.incoming.value,
                        ),
                    )
                }
                START_STICKY
            }
        }
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onDestroy() {
        notificationJob?.cancel()
        if (::controller.isInitialized) {
            controller.shutdown()
        }
        releaseMulticastLock()
        serviceScope.cancel()
        super.onDestroy()
    }

    inner class LocalBinder : Binder() {
        val service: AirBridgeService get() = this@AirBridgeService
    }

    // ------------------------------------------------------------- notification

    private fun showForeground(notification: Notification) {
        try {
            ServiceCompat.startForeground(
                this,
                NOTIFICATION_ID,
                notification,
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
                } else {
                    0
                },
            )
        } catch (t: Throwable) {
            // The service still runs while bound; it just cannot show the
            // ongoing card, so log and carry on instead of crashing.
            Log.e(TAG, "Unable to enter the foreground", t)
        }
    }

    private fun updateNotification(
        discovery: DiscoveryUiState,
        transfer: TransferUi?,
        incoming: IncomingTransferUi?,
    ) {
        val signature = buildString {
            append(discovery.starting).append('|')
            append(discovery.running).append('|')
            append(discovery.error).append('|')
            append(transfer?.phase).append('|')
            append(transfer?.percent).append('|')
            append(transfer?.fileName).append('|')
            append(transfer?.peerName).append('|')
            append(incoming?.id).append('|')
            append(incoming?.phase).append('|')
            append(incoming?.percent).append('|')
            append(incoming?.fileName).append('|')
            append(incoming?.savedName)
        }
        if (signature == lastSignature) return
        lastSignature = signature
        val manager = getSystemService(NotificationManager::class.java) ?: return
        manager.notify(NOTIFICATION_ID, buildNotification(discovery, transfer, incoming))
    }

    private fun buildNotification(
        discovery: DiscoveryUiState?,
        transfer: TransferUi?,
        incoming: IncomingTransferUi?,
    ): Notification {
        val text = when {
            incoming != null -> incomingText(incoming)
            transfer != null -> transferText(transfer)
            discovery?.error != null -> getString(R.string.notification_needs_attention)
            discovery?.starting == true -> getString(R.string.notification_starting)
            discovery?.running == true -> getString(R.string.notification_scanning)
            else -> getString(R.string.notification_stopped)
        }

        val openIntent = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )
        val stopIntent = PendingIntent.getService(
            this,
            1,
            Intent(this, AirBridgeService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        return NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.notification_title))
            .setContentText(text)
            .setContentIntent(openIntent)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setShowWhen(false)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .addAction(0, getString(R.string.notification_stop), stopIntent)
            .build()
    }

    private fun transferText(transfer: TransferUi): String = when (transfer.phase) {
        TransferPhase.PREPARING -> getString(R.string.notification_preparing)
        TransferPhase.CONNECTING -> getString(R.string.notification_connecting, transfer.peerName)
        TransferPhase.VERIFYING ->
            getString(R.string.notification_verify_transfer, transfer.peerName)
        TransferPhase.RUNNING, TransferPhase.CANCELLING -> {
            val percent = transfer.percent
            if (transfer.direction == ShareDirection.SENDING) {
                if (percent != null) {
                    getString(R.string.notification_sending, transfer.fileName, percent)
                } else {
                    getString(R.string.notification_sending_wait, transfer.fileName)
                }
            } else {
                if (percent != null) {
                    getString(R.string.notification_receiving, transfer.fileName, percent)
                } else {
                    getString(R.string.notification_receiving_wait, transfer.fileName)
                }
            }
        }
        TransferPhase.COMPLETED ->
            if (transfer.direction == ShareDirection.SENDING) {
                getString(R.string.notification_sent, transfer.fileName, transfer.peerName)
            } else {
                getString(R.string.notification_received, transfer.fileName)
            }
        TransferPhase.FAILED -> getString(R.string.notification_failed)
        TransferPhase.CANCELLED -> getString(R.string.notification_cancelled)
    }

    private fun incomingText(incoming: IncomingTransferUi): String = when (incoming.phase) {
        IncomingPhase.VERIFYING -> getString(R.string.notification_incoming_verify)
        IncomingPhase.WAITING -> getString(R.string.notification_incoming_offer)
        IncomingPhase.RECEIVING -> {
            val percent = incoming.percent
            if (percent != null) {
                getString(R.string.notification_incoming, incoming.fileName, percent)
            } else {
                getString(R.string.notification_incoming_wait, incoming.fileName)
            }
        }
        IncomingPhase.READY -> getString(R.string.notification_incoming_ready)
        IncomingPhase.SAVING -> getString(R.string.notification_incoming_saving)
        IncomingPhase.SAVED -> getString(
            R.string.notification_incoming_saved,
            incoming.savedName ?: incoming.fileName,
        )
        IncomingPhase.VERIFIED -> getString(R.string.notification_incoming_verified)
        IncomingPhase.FAILED -> getString(R.string.notification_incoming_failed)
        IncomingPhase.DECLINED -> getString(R.string.notification_cancelled)
    }

    private fun createNotificationChannel() {
        val manager = getSystemService(NotificationManager::class.java) ?: return
        val channel = NotificationChannel(
            CHANNEL_ID,
            getString(R.string.notification_channel_name),
            NotificationManager.IMPORTANCE_LOW,
        ).apply {
            description = getString(R.string.notification_channel_description)
            setShowBadge(false)
            lockscreenVisibility = Notification.VISIBILITY_PRIVATE
        }
        manager.createNotificationChannel(channel)
    }

    // ------------------------------------------------------------- Wi-Fi locks

    private fun acquireMulticastLock() {
        val manager = applicationContext.getSystemService(WifiManager::class.java) ?: return
        multicastLock = try {
            manager.createMulticastLock(MULTICAST_LOCK_TAG).apply {
                setReferenceCounted(false)
                acquire()
            }
        } catch (t: Throwable) {
            Log.w(TAG, "Multicast lock unavailable", t)
            null
        }
    }

    private fun releaseMulticastLock() {
        val lock = multicastLock ?: return
        multicastLock = null
        runCatching { if (lock.isHeld) lock.release() }
    }

    companion object {
        private const val TAG = "AirBridgeService"
        private const val CHANNEL_ID = "airbridge.sharing"
        private const val NOTIFICATION_ID = 42_517
        private const val MULTICAST_LOCK_TAG = "AirBridge:mdns"

        const val ACTION_START = "com.airbridge.app.action.START"
        const val ACTION_STOP = "com.airbridge.app.action.STOP"
    }
}
