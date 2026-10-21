package com.airbridge.app.core

import android.content.Context
import android.os.PowerManager
import android.util.Log
import com.airbridge.app.model.IncomingPhase
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.core.FfiException
import com.airbridge.core.FileTransferSession
import com.airbridge.core.ReceiveOptions
import com.airbridge.core.TransferEvent
import com.airbridge.core.TransferReport
import com.airbridge.core.events
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Long-lived inbound half of the transfer subsystem.
 *
 * The Rust discovery engine advertises `listenPort`, but it does not bind a
 * socket: binding is a property of a [FileTransferSession] created with
 * `receive`. This class keeps one such session bound to `0.0.0.0:42517`,
 * accepts exactly one peer at a time, and immediately re-arms after the
 * session ends. That is what turns the Windows sender's connection attempt
 * from "connection refused" into a transfer.
 *
 * # Consent and storage
 *
 * The FFI accept is a single step: the transport reads `HELLO`, starts
 * streaming, and writes into a destination path. There is no pre-accept
 * metadata callback and no raw-socket hand-off. To keep the user in control,
 * the payload is staged in the app's private cache and surfaced as an
 * [IncomingTransferUi] as soon as the transport reports `Started`. Bytes never
 * reach public Downloads until the user accepts; declining cancels the session
 * and deletes both the part file and its sidecar.
 *
 * # Threading
 *
 * The event pump and `run` are suspending FFI calls; this whole loop lives on
 * [Dispatchers.IO]. `accept()`/`decline()` are safe to call from the UI thread:
 * cancellation goes through the thread-safe Rust `cancel()` entry point, and
 * the small amount of shared state is guarded by [lock].
 */
class InboundReceiver(
    context: Context,
    private val scope: CoroutineScope,
    private val notify: (String) -> Unit,
) {
    private val appContext = context.applicationContext
    private val stagingDirectory = File(appContext.cacheDir, STAGING_DIRECTORY)
    private val store = IncomingFileStore(appContext)
    private val lock = Any()

    private val _incoming = MutableStateFlow<IncomingTransferUi?>(null)
    val incoming: StateFlow<IncomingTransferUi?> = _incoming.asStateFlow()

    private val running = AtomicBoolean(false)
    private var loopJob: Job? = null

    @Volatile
    private var activeSession: FileTransferSession? = null

    @Volatile
    private var currentOfferId: String? = null

    @Volatile
    private var currentStaging: File? = null

    /** Set when the user declined this offer, so a late completion cannot revive it. */
    @Volatile
    private var declinedOfferId: String? = null

    @Volatile
    private var consentJob: Job? = null

    @Volatile
    private var accepted = false

    /** True only between `run()` starting and resolving, not during idle binds. */
    @Volatile
    private var sessionActive = false

    @Volatile
    private var pendingDecision: CompletableDeferred<Boolean>? = null

    @Volatile
    private var lastListenerError: String? = null

    /** Held only while a peer is actually streaming, not during idle binds. */
    private var wakeLock: PowerManager.WakeLock? = null

    /** Binds the inbound port and keeps it bound until [stop]. Idempotent. */
    fun start() {
        if (!running.compareAndSet(false, true)) return
        cleanupStaging()
        loopJob = scope.launch(Dispatchers.IO) { receiveLoop() }
    }

    /** Stops the listener, cancels an in-flight receive and drops staging. */
    fun stop() {
        running.set(false)
        val session = activeSession
        runCatching { session?.cancel() }
        loopJob?.cancel()
        loopJob = null
        consentJob?.cancel()
        consentJob = null
        releaseWakeLock()

        synchronized(lock) {
            pendingDecision?.complete(false)
            pendingDecision = null
            accepted = false
            sessionActive = false
            declinedOfferId = currentOfferId
            _incoming.value = null
        }
        currentStaging?.let(::deleteStaging)
        currentStaging = null
        currentOfferId = null
        activeSession = null
    }

    /** User approved the current offer. */
    fun accept() {
        synchronized(lock) {
            val current = _incoming.value ?: return
            when (current.phase) {
                IncomingPhase.WAITING -> {
                    accepted = true
                    consentJob?.cancel()
                    consentJob = null
                    _incoming.value = current.copy(phase = IncomingPhase.RECEIVING)
                }
                IncomingPhase.READY -> {
                    accepted = true
                    _incoming.value = current.copy(
                        phase = IncomingPhase.SAVING,
                        error = null,
                    )
                    pendingDecision?.complete(true)
                }
                else -> Unit
            }
        }
    }

    /** User declined the offer, or cancelled an accepted transfer. */
    fun decline() {
        var session: FileTransferSession? = null
        var announced = false
        synchronized(lock) {
            val current = _incoming.value ?: return
            when (current.phase) {
                IncomingPhase.WAITING, IncomingPhase.RECEIVING -> {
                    accepted = false
                    declinedOfferId = current.id
                    session = activeSession
                    _incoming.value = null
                }
                IncomingPhase.READY -> {
                    accepted = false
                    declinedOfferId = current.id
                    pendingDecision?.complete(false)
                    _incoming.value = null
                }
                IncomingPhase.SAVED, IncomingPhase.FAILED, IncomingPhase.DECLINED -> {
                    _incoming.value = null
                }
                IncomingPhase.SAVING -> return
            }
            consentJob?.cancel()
            consentJob = null
            announced = true
        }
        if (announced) {
            runCatching { session?.cancel() }
        }
    }

    /** Clears a finished card after the user has read it. */
    fun dismiss() {
        synchronized(lock) {
            val current = _incoming.value ?: return
            if (current.phase.isTerminal) {
                _incoming.value = null
            }
        }
    }

    // --------------------------------------------------------------- receiver

    private suspend fun receiveLoop() {
        stagingDirectory.mkdirs()
        var retryDelay = RETRY_FAST_MS

        while (running.get() && currentCoroutineContext().isActive) {
            if (!NativeCore.status().available) {
                Log.w(TAG, "Inbound listener not started: native core unavailable")
                return
            }

            val staging = File(stagingDirectory, "incoming-${UUID.randomUUID()}.bin")
            val offerId = UUID.randomUUID().toString()
            val announced = AtomicBoolean(false)
            var session: FileTransferSession? = null
            var slowRetry = false

            try {
                session = withContext(Dispatchers.IO) {
                    FileTransferSession.receive(
                        "0.0.0.0:$TRANSFER_PORT",
                        staging.absolutePath,
                        ReceiveOptions(
                            expectedPayloadId = null,
                            ackEveryBytes = ACK_EVERY_BYTES.toULong(),
                            verifyWholeFile = true,
                            acceptTimeoutMs = ACCEPT_TIMEOUT_MS.toULong(),
                            progressIntervalMs = PROGRESS_INTERVAL_MS.toULong(),
                        ),
                    )
                }
                install(session, offerId, staging)

                // Mark the session active before the pump starts, so an early
                // `Started` cannot miss the wake lock or consent timeout.
                sessionActive = true
                val report = coroutineScope {
                    val collector = launch {
                        session.events().collect { event ->
                            onTransferEvent(offerId, staging, announced, event)
                        }
                    }
                    try {
                        session.run()
                    } finally {
                        sessionActive = false
                        collector.cancel()
                        releaseWakeLock()
                    }
                }

                lastListenerError = null
                retryDelay = RETRY_FAST_MS
                finishCompleted(offerId, staging, announced, report)
            } catch (c: CancellationException) {
                throw c
            } catch (t: Throwable) {
                slowRetry = finishFailed(offerId, staging, announced, t)
                retryDelay = if (slowRetry) RETRY_SLOW_MS else RETRY_FAST_MS
            } finally {
                releaseWakeLock()
                clearSession()
                runCatching { session?.close() }
                deleteStaging(staging)
            }

            if (!running.get()) break
            delay(retryDelay)
        }
    }

    private fun install(
        session: FileTransferSession,
        offerId: String,
        staging: File,
    ) {
        synchronized(lock) {
            activeSession = session
            currentOfferId = offerId
            currentStaging = staging
            accepted = false
            sessionActive = false
            declinedOfferId = null
            pendingDecision = null
            consentJob?.cancel()
            consentJob = null
        }
    }

    private fun clearSession() {
        synchronized(lock) {
            activeSession = null
            currentOfferId = null
            currentStaging = null
            accepted = false
            sessionActive = false
            pendingDecision = null
            consentJob?.cancel()
            consentJob = null
        }
    }

    private fun onTransferEvent(
        offerId: String,
        staging: File,
        announced: AtomicBoolean,
        event: TransferEvent,
    ) {
        when (event) {
            is TransferEvent.Started -> {
                ensureOffer(offerId, staging, announced, event.totalBytes?.toLong())
            }
            is TransferEvent.Progress -> {
                ensureOffer(offerId, staging, announced, event.totalBytes?.toLong())
                val needsTotal = _incoming.value
                    ?.let { it.id == offerId && it.totalBytes == null } == true
                val hint = event.totalBytes?.toLong()
                    ?: if (needsTotal) InboundResumeHints.totalBytes(staging) else null
                val moved = event.bytesTransferred.toLong()
                _incoming.update { current ->
                    if (current?.id != offerId || current.phase.isTerminal) {
                        current
                    } else {
                        current.copy(
                            bytesTransferred = maxOf(current.bytesTransferred, moved),
                            totalBytes = current.totalBytes ?: hint,
                            phase = if (accepted && current.phase == IncomingPhase.WAITING) {
                                IncomingPhase.RECEIVING
                            } else {
                                current.phase
                            },
                        )
                    }
                }
            }
            // Terminal events are applied from the authoritative `run` result.
            is TransferEvent.Completed, is TransferEvent.Failed,
            TransferEvent.Cancelled, is TransferEvent.Lagged -> Unit
        }
    }

    private fun ensureOffer(
        offerId: String,
        staging: File,
        announced: AtomicBoolean,
        reportedTotal: Long?,
    ) {
        val total = reportedTotal ?: InboundResumeHints.totalBytes(staging)
        if (announced.compareAndSet(false, true)) {
            var waiting = false
            synchronized(lock) {
                if (currentOfferId != offerId) return
                if (declinedOfferId == offerId) return
                val existing = _incoming.value
                if (existing?.id == offerId &&
                    existing.phase != IncomingPhase.WAITING &&
                    existing.phase != IncomingPhase.RECEIVING
                ) {
                    // A fast transfer may finish before this event is pumped.
                    // Never regress READY/SAVING/terminal back to WAITING.
                    return
                }
                waiting = !accepted
                _incoming.value = IncomingTransferUi(
                    id = offerId,
                    totalBytes = total,
                    phase = if (accepted) IncomingPhase.RECEIVING else IncomingPhase.WAITING,
                )
            }
            if (!sessionActive) return
            acquireWakeLock()
            if (!sessionActive) {
                releaseWakeLock()
                return
            }
            if (waiting) scheduleConsentTimeout(offerId)
        } else if (total != null) {
            _incoming.update { current ->
                if (current?.id == offerId && current.totalBytes == null) {
                    current.copy(totalBytes = total)
                } else {
                    current
                }
            }
        }
    }

    private fun scheduleConsentTimeout(offerId: String) {
        if (!running.get()) return
        consentJob?.cancel()
        consentJob = scope.launch(Dispatchers.IO) {
            delay(CONSENT_TIMEOUT_MS)
            declineIfWaiting(offerId)
        }
    }

    private fun declineIfWaiting(offerId: String) {
        var session: FileTransferSession? = null
        var expired = false
        synchronized(lock) {
            val current = _incoming.value
            if (accepted || current?.id != offerId || current.phase != IncomingPhase.WAITING) {
                return
            }
            accepted = false
            session = activeSession
            declinedOfferId = offerId
            _incoming.value = null
            expired = true
        }
        if (expired) notify("Incoming transfer expired")
        runCatching { session?.cancel() }
    }

    // --------------------------------------------------------------- terminal

    private suspend fun finishCompleted(
        offerId: String,
        staging: File,
        announced: AtomicBoolean,
        report: TransferReport,
    ) {
        announced.set(true)
        consentJob?.cancel()
        consentJob = null
        lastListenerError = null

        val total = report.totalBytes.toLong().coerceAtLeast(0L)
        val extension = withContext(Dispatchers.IO) { IncomingFileKinds.sniffExtension(staging) }
        val name = IncomingNames.friendly(report.payloadId, extension = extension)

        // Display READY and install the decision handle under one lock, so a
        // Discard tap cannot land between "card shown" and "decision awaited"
        // and leave the listener blocked forever.
        var approvedNow = false
        var decision: CompletableDeferred<Boolean>? = null
        synchronized(lock) {
            if (currentOfferId != offerId) return
            if (declinedOfferId == offerId) {
                _incoming.value = null
                return
            }
            val current = _incoming.value?.takeIf { it.id == offerId }
                ?: IncomingTransferUi(id = offerId)
            if (accepted) {
                _incoming.value = current.copy(
                    fileName = name,
                    bytesTransferred = total,
                    totalBytes = total.takeIf { it > 0L } ?: current.totalBytes,
                    phase = IncomingPhase.SAVING,
                    error = null,
                )
                approvedNow = true
            } else {
                decision = CompletableDeferred()
                pendingDecision = decision
                _incoming.value = current.copy(
                    fileName = name,
                    bytesTransferred = total,
                    totalBytes = total.takeIf { it > 0L } ?: current.totalBytes,
                    phase = IncomingPhase.READY,
                    error = null,
                )
            }
        }
        if (!approvedNow) {
            val approved = decision?.await() ?: return
            synchronized(lock) {
                if (pendingDecision === decision) pendingDecision = null
                accepted = approved
            }
            if (!approved) {
                synchronized(lock) {
                    if (_incoming.value?.id == offerId) _incoming.value = null
                }
                return
            }
        }

        // Copy into Downloads, allowing a retry after a transient failure such
        // as the legacy storage prompt being declined once.
        while (true) {
            setPhase(offerId, IncomingPhase.SAVING, error = null)
            val save = withContext(Dispatchers.IO) {
                runCatching { store.save(staging, name) }
            }
            val saved = save.getOrNull()
            if (saved != null) {
                synchronized(lock) {
                    val current = _incoming.value
                    if (current?.id == offerId) {
                        _incoming.value = current.copy(
                            fileName = saved.name,
                            bytesTransferred = total,
                            totalBytes = total.takeIf { it > 0L } ?: current.totalBytes,
                            phase = IncomingPhase.SAVED,
                            error = null,
                            savedName = saved.name,
                            savedUri = saved.uri,
                        )
                    }
                }
                notify("Saved ${saved.name} to Downloads")
                scheduleAutoDismiss(offerId)
                return
            }

            val error = friendlyError(save.exceptionOrNull())
            var retryDecision: CompletableDeferred<Boolean>? = null
            synchronized(lock) {
                accepted = false
                val current = _incoming.value
                if (current?.id == offerId) {
                    _incoming.value = current.copy(
                        phase = IncomingPhase.READY,
                        error = error,
                    )
                }
                retryDecision = CompletableDeferred()
                pendingDecision = retryDecision
            }
            val retry = retryDecision?.await() ?: return
            synchronized(lock) {
                if (pendingDecision === retryDecision) pendingDecision = null
                accepted = retry
            }
            if (!retry) {
                synchronized(lock) {
                    if (_incoming.value?.id == offerId) _incoming.value = null
                }
                return
            }
        }
    }

    /**
     * Handles an unsuccessful session.
     *
     * @return true when the listener should back off before rebinding (bind
     *   failures and other pre-offer I/O errors).
     */
    private fun finishFailed(
        offerId: String,
        staging: File,
        announced: AtomicBoolean,
        error: Throwable,
    ): Boolean {
        consentJob?.cancel()
        consentJob = null

        val cancelled = error is FfiException.Cancelled
        val current = _incoming.value
        val hadOffer = announced.get() || current?.id == offerId

        if (!hadOffer) {
            if (error is FfiException.Timeout) {
                // Nobody connected during the accept window. Re-arm quietly.
                lastListenerError = null
                return false
            }
            if (error is FfiException.Io) {
                val message = friendlyError(error)
                if (message != lastListenerError) {
                    lastListenerError = message
                    notify("Incoming transfers are unavailable: $message")
                }
                Log.w(TAG, "Inbound listener failed before accepting a peer", error)
                return true
            }
            Log.w(TAG, "Ignored an invalid inbound connection", error)
            return false
        }

        if (cancelled || current?.phase == IncomingPhase.DECLINED) {
            synchronized(lock) {
                if (_incoming.value?.id == offerId) _incoming.value = null
            }
            return false
        }

        synchronized(lock) {
            val active = _incoming.value
            if (active?.id == offerId) {
                _incoming.value = active.copy(
                    phase = IncomingPhase.FAILED,
                    error = friendlyError(error),
                )
            }
        }
        notify("Incoming transfer failed")
        scheduleAutoDismiss(offerId)
        deleteStaging(staging)
        return false
    }

    private fun setPhase(offerId: String, phase: IncomingPhase, error: String?) {
        synchronized(lock) {
            val current = _incoming.value
            if (current?.id == offerId) {
                _incoming.value = current.copy(phase = phase, error = error)
            }
        }
    }

    private fun scheduleAutoDismiss(offerId: String) {
        scope.launch(Dispatchers.IO) {
            delay(TERMINAL_CARD_VISIBLE_MS)
            synchronized(lock) {
                val current = _incoming.value
                if (current?.id == offerId &&
                    (current.phase == IncomingPhase.SAVED || current.phase == IncomingPhase.FAILED)
                ) {
                    _incoming.value = null
                }
            }
        }
    }

    // ---------------------------------------------------------------- storage

    /** Keeps the CPU awake while frames are in flight; idle binds stay unlocked. */
    private fun acquireWakeLock() {
        synchronized(lock) {
            releaseWakeLockLocked()
            val manager = appContext.getSystemService(PowerManager::class.java) ?: return
            wakeLock = runCatching {
                manager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG).apply {
                    setReferenceCounted(false)
                    acquire(WAKE_LOCK_TIMEOUT_MS)
                }
            }.getOrNull()
        }
    }

    private fun releaseWakeLock() {
        synchronized(lock) { releaseWakeLockLocked() }
    }

    private fun releaseWakeLockLocked() {
        val held = wakeLock ?: return
        wakeLock = null
        runCatching { if (held.isHeld) held.release() }
    }

    private fun cleanupStaging() {
        runCatching {
            stagingDirectory.listFiles()?.forEach { file -> file.delete() }
        }
    }

    private fun deleteStaging(staging: File) {
        runCatching { staging.delete() }
        runCatching { File(staging.absolutePath + ".part").delete() }
        runCatching { File(staging.absolutePath + ".part.state").delete() }
    }

    private fun friendlyError(error: Throwable?): String = when (error) {
        null -> "The transfer failed"
        is StoragePermissionRequiredException ->
            "Allow storage access to save files to Downloads"
        is FfiException.Timeout -> error.message.ifBlank { "The transfer timed out" }
        is FfiException.PeerClosed -> "The other device disconnected"
        is FfiException.PeerAborted -> "The other device cancelled the transfer"
        is FfiException.ChecksumMismatch -> "A checksum did not match"
        is FfiException.Io -> error.message.ifBlank { "A network error interrupted the transfer" }
        is FfiException.Protocol -> "The other device sent invalid data"
        is UnsatisfiedLinkError -> "The native core is missing for this device's ABI"
        is IOException -> error.message ?: "A file error interrupted the transfer"
        else -> error.message?.takeIf { it.isNotBlank() } ?: error.javaClass.simpleName
    }

    companion object {
        private const val TAG = "InboundReceiver"

        /** Keep in sync with the Rust discovery default and the Windows peer. */
        const val TRANSFER_PORT = 42_517

        private const val STAGING_DIRECTORY = "incoming"
        private const val ACCEPT_TIMEOUT_MS = 120_000L
        private const val ACK_EVERY_BYTES = 4L * 1024L * 1024L
        private const val PROGRESS_INTERVAL_MS = 100L
        private const val CONSENT_TIMEOUT_MS = 120_000L
        private const val RETRY_FAST_MS = 500L
        private const val RETRY_SLOW_MS = 5_000L
        private const val TERMINAL_CARD_VISIBLE_MS = 8_000L
        private const val WAKE_LOCK_TAG = "AirBridge:inbound"
        private const val WAKE_LOCK_TIMEOUT_MS = 15L * 60L * 1000L
    }
}
