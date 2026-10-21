package com.airbridge.app.core

import android.content.Context
import android.os.PowerManager
import android.util.Log
import com.airbridge.app.model.IncomingFilePhase
import com.airbridge.app.model.IncomingFileUi
import com.airbridge.app.model.IncomingPhase
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.core.BatchEvent
import com.airbridge.core.BatchItemInfo
import com.airbridge.core.BatchItemStatus
import com.airbridge.core.BatchOptions
import com.airbridge.core.BatchPhase
import com.airbridge.core.BatchReport
import com.airbridge.core.BatchSnapshot
import com.airbridge.core.BatchTransferSession
import com.airbridge.core.FfiException
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
 * socket: binding is a property of a [BatchTransferSession] created with
 * `receive`. This class keeps one such session bound to `0.0.0.0:42517`,
 * accepts exactly one peer at a time, and immediately re-arms after the
 * session ends. That is what turns the Windows sender's connection attempt
 * from "connection refused" into a transfer.
 *
 * # Consent and storage
 *
 * The FFI accept is a single step: the transport reads the manifest, starts
 * streaming, and writes every file into a per-offer staging directory. There
 * is no pre-accept metadata callback and no raw-socket hand-off. To keep the
 * user in control, the payload is staged in the app's private cache and
 * surfaced as an [IncomingTransferUi] as soon as the transport reports the
 * manifest. Bytes never reach public Downloads until the user accepts; a
 * verified batch is then copied into `Downloads/AirBridge`, preserving the
 * manifest's folder structure. Declining cancels the session and deletes the
 * whole staging tree.
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
    private val preferences: AppPreferences,
    private val notify: (String) -> Unit,
    private val onPeerVerified: (peerId: String, fingerprint: String) -> Unit,
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
    private var activeSession: BatchTransferSession? = null

    @Volatile
    private var currentOfferId: String? = null

    @Volatile
    private var currentStaging: File? = null

    @Volatile
    private var currentPeerId: String? = null

    @Volatile
    private var currentPeerFingerprint: String? = null

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
        currentPeerId = null
        currentPeerFingerprint = null
        activeSession = null
    }

    /** User approved the current offer. */
    fun accept() {
        var verificationSession: BatchTransferSession? = null
        synchronized(lock) {
            val current = _incoming.value ?: return
            when (current.phase) {
                IncomingPhase.VERIFYING -> {
                    if (current.verificationSent) return
                    // The user has both confirmed the device and accepted the
                    // files. Remember that before releasing the channel: the
                    // manifest follows immediately and must not regress this
                    // card back to the file-offer prompt.
                    accepted = true
                    _incoming.value = current.copy(
                        verificationSent = true,
                        error = null,
                    )
                    verificationSession = activeSession
                }
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
                IncomingPhase.VERIFIED, IncomingPhase.SAVED,
                IncomingPhase.FAILED, IncomingPhase.DECLINED -> {
                    _incoming.value = null
                }
                else -> Unit
            }
        }
        val session = verificationSession ?: return
        val failure = runCatching { session.confirmPeer() }.exceptionOrNull()
        if (failure != null) {
            synchronized(lock) {
                val current = _incoming.value
                if (current?.phase == IncomingPhase.VERIFYING) {
                    accepted = false
                    _incoming.value = current.copy(
                        verificationSent = false,
                        error = friendlyError(failure),
                    )
                }
            }
        }
    }

    /** User declined the offer, or cancelled an accepted transfer. */
    fun decline() {
        var session: BatchTransferSession? = null
        var announced = false
        synchronized(lock) {
            val current = _incoming.value ?: return
            when (current.phase) {
                IncomingPhase.VERIFYING -> {
                    accepted = false
                    declinedOfferId = current.id
                    session = activeSession
                    _incoming.value = null
                }
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
                IncomingPhase.VERIFIED, IncomingPhase.SAVED,
                IncomingPhase.FAILED, IncomingPhase.DECLINED -> {
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

            val staging = File(stagingDirectory, "incoming-${UUID.randomUUID()}")
            if (!staging.exists() && !staging.mkdirs()) {
                Log.w(TAG, "Could not create the inbound staging directory")
                delay(RETRY_SLOW_MS)
                continue
            }

            val offerId = UUID.randomUUID().toString()
            val announced = AtomicBoolean(false)
            var session: BatchTransferSession? = null
            var slowRetry = false

            try {
                session = withContext(Dispatchers.IO) {
                    BatchTransferSession.receive(
                        "0.0.0.0:$TRANSFER_PORT",
                        staging.absolutePath,
                        batchOptions(),
                    )
                }
                install(session, offerId, staging)

                // Mark the session active before the pump starts, so an early
                // manifest cannot miss the wake lock or consent timeout.
                sessionActive = true
                val report = coroutineScope {
                    val collector = launch {
                        session.events().collect { event ->
                            onBatchEvent(offerId, announced, event, session)
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
                finishCompleted(offerId, staging, announced, report, session)
            } catch (c: CancellationException) {
                throw c
            } catch (t: Throwable) {
                slowRetry = finishFailed(offerId, staging, announced, session, t)
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

    private fun batchOptions(): BatchOptions {
        return BatchOptions(
            identityPkcs8 = preferences.identityPkcs8,
            trustedPeerFingerprints = preferences.trustedFingerprints().toList(),
            ackEveryBytes = ACK_EVERY_BYTES.toULong(),
            verifyWholeFile = true,
            responseTimeoutMs = RESPONSE_TIMEOUT_MS.toULong(),
            acceptTimeoutMs = ACCEPT_TIMEOUT_MS.toULong(),
            progressIntervalMs = PROGRESS_INTERVAL_MS.toULong(),
            localPeerId = preferences.peerIdHex,
        )
    }

    private fun install(
        session: BatchTransferSession,
        offerId: String,
        staging: File,
    ) {
        synchronized(lock) {
            activeSession = session
            currentOfferId = offerId
            currentStaging = staging
            currentPeerId = null
            currentPeerFingerprint = null
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
            currentPeerId = null
            currentPeerFingerprint = null
            accepted = false
            sessionActive = false
            pendingDecision = null
            consentJob?.cancel()
            consentJob = null
        }
    }

    private fun onBatchEvent(
        offerId: String,
        announced: AtomicBoolean,
        event: BatchEvent,
        session: BatchTransferSession,
    ) {
        when (event) {
            is BatchEvent.VerificationRequired -> {
                val shown = synchronized(lock) {
                    if (currentOfferId != offerId || declinedOfferId == offerId) {
                        false
                    } else {
                        val current = _incoming.value?.takeIf { it.id == offerId }
                            ?: IncomingTransferUi(id = offerId)
                        _incoming.value = current.copy(
                            phase = IncomingPhase.VERIFYING,
                            sas = event.sas,
                            peerFingerprint = event.peerFingerprint,
                            error = null,
                            verificationSent = false,
                        )
                        true
                    }
                }
                if (shown && sessionActive) acquireWakeLock()
            }
            is BatchEvent.PeerVerified -> {
                val active = synchronized(lock) {
                    if (currentOfferId != offerId || declinedOfferId == offerId) {
                        false
                    } else {
                        currentPeerId = event.peerId
                        currentPeerFingerprint = event.peerFingerprint
                        val current = _incoming.value?.takeIf { it.id == offerId }
                        if (current != null) {
                            _incoming.value = current.copy(
                                peerIdHex = event.peerId,
                                peerFingerprint = event.peerFingerprint,
                            )
                        }
                        true
                    }
                }
                if (active) onPeerVerified(event.peerId, event.peerFingerprint)
            }
            is BatchEvent.Started -> {
                val snapshot = session.snapshot()
                ensureOffer(offerId, announced, snapshot, event.totalBytes.toLong())
            }
            is BatchEvent.FileStarted -> {
                updateRow(offerId, event.index.toInt()) { row ->
                    row.copy(
                        size = event.size.toLong().takeIf { it > 0L } ?: row.size,
                        bytesTransferred = 0L,
                        phase = IncomingFilePhase.ACTIVE,
                        error = null,
                    )
                }
            }
            is BatchEvent.Progress -> {
                if (!announced.get()) {
                    ensureOffer(offerId, announced, session.snapshot(), event.totalBytes.toLong())
                }
                val activeIndex = event.currentFileIndex?.toInt()
                _incoming.update { current ->
                    if (current?.id != offerId || current.phase.isTerminal) {
                        current
                    } else {
                        current.copy(
                            bytesTransferred = maxOf(
                                current.bytesTransferred,
                                event.bytesTransferred.toLong(),
                            ),
                            totalBytes = event.totalBytes.toLong().takeIf { it > 0L }
                                ?: current.totalBytes,
                            etaMillis = event.estimatedRemainingMs?.toLong(),
                            phase = if (accepted && current.phase == IncomingPhase.WAITING) {
                                IncomingPhase.RECEIVING
                            } else {
                                current.phase
                            },
                            files = if (activeIndex == null) {
                                current.files
                            } else {
                                current.files.map { row ->
                                    if (row.index == activeIndex) {
                                        row.copy(
                                            size = event.fileSize.toLong().takeIf { it > 0L }
                                                ?: row.size,
                                            bytesTransferred =
                                                event.fileBytesTransferred.toLong(),
                                            phase = IncomingFilePhase.ACTIVE,
                                        )
                                    } else {
                                        row
                                    }
                                }
                            },
                        )
                    }
                }
            }
            is BatchEvent.FileCompleted -> {
                updateRow(offerId, event.index.toInt()) { row ->
                    row.copy(
                        size = event.size.toLong().takeIf { it > 0L } ?: row.size,
                        bytesTransferred = event.size.toLong(),
                        phase = IncomingFilePhase.COMPLETED,
                        error = null,
                    )
                }
            }
            is BatchEvent.FileFailed -> {
                updateRow(offerId, event.index.toInt()) { row ->
                    row.copy(
                        phase = IncomingFilePhase.FAILED,
                        error = event.message.ifBlank { event.kind },
                    )
                }
            }
            // The authoritative result comes from `run`; see finishCompleted.
            is BatchEvent.Finished -> Unit
            is BatchEvent.Failed -> {
                if (declinedOfferId != offerId) {
                    markFailed(friendlyError(event))
                }
            }
            BatchEvent.Cancelled -> Unit
            is BatchEvent.Lagged -> {
                val snapshot = session.snapshot()
                if (!announced.get() && snapshot.totalFiles > 0u) {
                    ensureOffer(offerId, announced, snapshot, snapshot.totalBytes.toLong())
                }
                applySnapshot(offerId, snapshot)
            }
        }
    }

    /** Builds the offer card from the manifest snapshot. */
    private fun ensureOffer(
        offerId: String,
        announced: AtomicBoolean,
        snapshot: BatchSnapshot,
        reportedTotal: Long?,
    ) {
        val total = reportedTotal
            ?.takeIf { it > 0L }
            ?: snapshot.totalBytes.toLong().takeIf { it > 0L }
        if (announced.compareAndSet(false, true)) {
            var waiting = false
            synchronized(lock) {
                if (currentOfferId != offerId) return
                if (declinedOfferId == offerId) return
                val existing = _incoming.value
                if (existing?.id == offerId &&
                    existing.phase != IncomingPhase.VERIFYING &&
                    existing.phase != IncomingPhase.WAITING &&
                    existing.phase != IncomingPhase.RECEIVING
                ) {
                    // A fast transfer may finish before this event is pumped.
                    // Never regress READY/SAVING/terminal back to WAITING.
                    return
                }
                waiting = !accepted
                val rows = snapshot.items.map(::rowFromItem)
                val base = _incoming.value?.takeIf { it.id == offerId }
                    ?: IncomingTransferUi(id = offerId)
                _incoming.value = base.copy(
                    fileName = batchTitle(rows),
                    files = rows.ifEmpty { base.files },
                    totalBytes = total,
                    bytesTransferred = maxOf(
                        base.bytesTransferred,
                        snapshot.transferredBytes.toLong(),
                    ),
                    batchId = snapshot.batchId.takeIf { it != 0UL } ?: base.batchId,
                    phase = if (accepted) IncomingPhase.RECEIVING else IncomingPhase.WAITING,
                    verificationSent = false,
                    error = null,
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

    private fun applySnapshot(offerId: String, snapshot: BatchSnapshot) {
        _incoming.update { current ->
            if (current?.id != offerId || current.phase.isTerminal) {
                current
            } else {
                val previous = current.files.associateBy { it.index }
                current.copy(
                    files = snapshot.items.map { item ->
                        val merged = rowFromItem(item)
                        val old = previous[merged.index]
                        if (old?.savedUri != null) {
                            merged.copy(
                                savedName = old.savedName,
                                savedUri = old.savedUri,
                            )
                        } else {
                            merged
                        }
                    },
                    totalBytes = snapshot.totalBytes.toLong().takeIf { it > 0L }
                        ?: current.totalBytes,
                    bytesTransferred = maxOf(
                        current.bytesTransferred,
                        snapshot.transferredBytes.toLong(),
                    ),
                    etaMillis = snapshot.estimatedRemainingMs?.toLong(),
                    batchId = snapshot.batchId.takeIf { it != 0UL } ?: current.batchId,
                )
            }
        }
    }

    private fun updateRow(
        offerId: String,
        index: Int,
        transform: (IncomingFileUi) -> IncomingFileUi,
    ) {
        _incoming.update { current ->
            if (current?.id != offerId || current.phase.isTerminal) {
                current
            } else {
                current.copy(
                    files = current.files.map { row ->
                        if (row.index == index) transform(row) else row
                    },
                )
            }
        }
    }

    private fun rowFromItem(item: BatchItemInfo): IncomingFileUi = IncomingFileUi(
        index = item.index.toInt(),
        relativePath = item.relativePath,
        size = item.size.toLong(),
        bytesTransferred = item.bytesTransferred.toLong(),
        phase = item.status.toFilePhase(),
        error = item.error,
    )

    private fun BatchItemStatus.toFilePhase(): IncomingFilePhase = when (this) {
        BatchItemStatus.PENDING -> IncomingFilePhase.PENDING
        BatchItemStatus.ACTIVE -> IncomingFilePhase.ACTIVE
        BatchItemStatus.COMPLETED -> IncomingFilePhase.COMPLETED
        BatchItemStatus.FAILED -> IncomingFilePhase.FAILED
        BatchItemStatus.CANCELLED -> IncomingFilePhase.CANCELLED
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
        var session: BatchTransferSession? = null
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
        report: BatchReport,
        session: BatchTransferSession,
    ) {
        announced.set(true)
        consentJob?.cancel()
        consentJob = null
        lastListenerError = null

        if (report.totalFiles == 0u && report.files.isEmpty()) {
            finishPairingOnly(offerId, report, session)
            return
        }

        if (declinedOfferId == offerId) {
            synchronized(lock) {
                if (_incoming.value?.id == offerId) _incoming.value = null
            }
            return
        }

        val rows = mergeReportRows(offerId, report)
        val completed = report.completedFiles.toInt()

        if (report.status == BatchPhase.CANCELLED) {
            synchronized(lock) {
                if (_incoming.value?.id == offerId) _incoming.value = null
            }
            return
        }

        if (completed == 0) {
            val message = rows.firstNotNullOfOrNull { row ->
                row.error?.takeIf { row.phase == IncomingFilePhase.FAILED }
            } ?: "No files could be received"
            synchronized(lock) {
                val current = _incoming.value?.takeIf { it.id == offerId }
                    ?: IncomingTransferUi(id = offerId)
                _incoming.value = current.copy(
                    fileName = batchTitle(rows),
                    files = rows,
                    phase = IncomingPhase.FAILED,
                    totalBytes = report.totalBytes.toLong().takeIf { it > 0L },
                    bytesTransferred = report.transferredBytes.toLong(),
                    etaMillis = null,
                    batchId = report.batchId,
                    sas = null,
                    error = message,
                )
            }
            notify("Incoming transfer failed")
            scheduleAutoDismiss(offerId)
            return
        }

        // Display READY and install the decision handle under one lock, so a
        // Discard tap cannot land between "card shown" and "decision awaited"
        // and leave the receiver blocked forever.
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
            val updated = current.copy(
                fileName = batchTitle(rows),
                files = rows,
                bytesTransferred = report.transferredBytes.toLong(),
                totalBytes = report.totalBytes.toLong().takeIf { it > 0L }
                    ?: current.totalBytes,
                batchId = report.batchId,
                etaMillis = null,
                sas = null,
                error = null,
            )
            if (accepted) {
                _incoming.value = updated.copy(phase = IncomingPhase.SAVING)
                approvedNow = true
            } else {
                decision = CompletableDeferred()
                pendingDecision = decision
                _incoming.value = updated.copy(phase = IncomingPhase.READY)
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

        saveBatch(offerId, staging, report)
    }

    /**
     * Completes a pairing-only connection: the peer was authenticated, but
     * there is no file to offer or save. Pairing and transfers share the
     * listener port, so this is a normal, non-error outcome.
     */
    private fun finishPairingOnly(
        offerId: String,
        report: BatchReport,
        session: BatchTransferSession,
    ) {
        val peerId = session.peerId() ?: currentPeerId
        val fingerprint = session.peerFingerprint() ?: currentPeerFingerprint
        if (peerId != null && fingerprint != null) {
            onPeerVerified(peerId, fingerprint)
        }
        synchronized(lock) {
            if (currentOfferId != offerId) return
            if (declinedOfferId == offerId) {
                _incoming.value = null
                return
            }
            val current = _incoming.value?.takeIf { it.id == offerId }
                ?: IncomingTransferUi(id = offerId)
            _incoming.value = current.copy(
                fileName = "Device verification",
                phase = IncomingPhase.VERIFIED,
                pairingOnly = true,
                files = emptyList(),
                peerIdHex = peerId ?: current.peerIdHex,
                peerFingerprint = fingerprint ?: current.peerFingerprint,
                bytesTransferred = 0,
                totalBytes = null,
                batchId = null,
                etaMillis = null,
                error = null,
                verificationSent = false,
            )
        }
        notify("Device verified")
        scheduleAutoDismiss(offerId)
    }

    /**
     * Copies every verified file from staging into public Downloads.
     *
     * The first pass runs without another prompt when the user accepted the
     * incoming offer. A save failure (for example the API 26-28 storage grant)
     * returns the card to READY with an error and waits for a retry, preserving
     * files that were already committed.
     */
    private suspend fun saveBatch(
        offerId: String,
        staging: File,
        report: BatchReport,
    ) {
        val completedIndexes = report.files
            .filter { it.status == BatchItemStatus.COMPLETED }
            .map { it.index }
            .toSet()

        while (true) {
            setPhase(offerId, IncomingPhase.SAVING, error = null)
            val savedIndexes = _incoming.value
                ?.takeIf { it.id == offerId }
                ?.files
                ?.filter { it.savedUri != null }
                ?.map { it.index.toUInt() }
                ?.toSet()
                ?: emptySet()
            val remaining = report.files.filter { item ->
                item.index in completedIndexes && item.index !in savedIndexes
            }

            if (remaining.isEmpty()) {
                finishSaved(offerId, report)
                return
            }

            var lastFailure: Throwable? = null
            for (item in remaining) {
                val source = resolveStagedSource(staging, item)
                updateRow(offerId, item.index.toInt()) { row ->
                    row.copy(phase = IncomingFilePhase.ACTIVE, error = null)
                }
                val save = withContext(Dispatchers.IO) {
                    runCatching {
                        store.save(
                            source = source,
                            displayName = item.relativePath.substringAfterLast('/'),
                            relativePath = item.relativePath,
                        )
                    }
                }
                val saved = save.getOrNull()
                if (saved != null) {
                    updateRow(offerId, item.index.toInt()) { row ->
                        row.copy(
                            phase = IncomingFilePhase.COMPLETED,
                            bytesTransferred = item.size.toLong(),
                            savedName = saved.name,
                            savedUri = saved.uri,
                            error = null,
                        )
                    }
                    continue
                }

                val failure = save.exceptionOrNull()
                lastFailure = failure
                updateRow(offerId, item.index.toInt()) { row ->
                    row.copy(
                        phase = IncomingFilePhase.FAILED,
                        error = friendlyError(failure),
                    )
                }
                // A missing storage grant fails every subsequent write too.
                if (failure is StoragePermissionRequiredException) break
            }

            val current = _incoming.value?.takeIf { it.id == offerId }
            val savedRows = current?.files?.filter { it.savedUri != null }.orEmpty()
            if (savedRows.size >= completedIndexes.size) {
                finishSaved(offerId, report)
                return
            }

            var retryDecision: CompletableDeferred<Boolean>? = null
            synchronized(lock) {
                accepted = false
                val visible = _incoming.value?.takeIf { it.id == offerId }
                if (visible != null) {
                    _incoming.value = visible.copy(
                        phase = IncomingPhase.READY,
                        etaMillis = null,
                        error = lastFailure?.let(::friendlyError)
                            ?: if (savedRows.isEmpty()) {
                                "The received files could not be saved"
                            } else {
                                "Saved ${savedRows.size} of ${completedIndexes.size} files"
                            },
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

    private fun finishSaved(offerId: String, report: BatchReport) {
        var savedCount = 0
        var savedName: String? = null
        var savedUri: String? = null
        synchronized(lock) {
            val current = _incoming.value?.takeIf { it.id == offerId } ?: return
            val saved = current.files.filter { it.savedUri != null }
            savedCount = saved.size
            savedName = if (saved.size == 1) {
                saved.first().savedName ?: saved.first().name
            } else {
                "$savedCount files"
            }
            savedUri = saved.firstOrNull()?.savedUri
            _incoming.value = current.copy(
                phase = IncomingPhase.SAVED,
                fileName = savedName ?: current.fileName,
                savedName = savedName,
                savedUri = savedUri,
                files = current.files.map { row ->
                    if (row.phase == IncomingFilePhase.ACTIVE) {
                        row.copy(phase = IncomingFilePhase.CANCELLED)
                    } else {
                        row
                    }
                },
                bytesTransferred = report.transferredBytes.toLong()
                    .takeIf { it > 0L } ?: current.bytesTransferred,
                etaMillis = null,
                error = null,
            )
        }
        notify(
            if (savedCount == 1) {
                "Saved ${savedName ?: "file"} to Downloads"
            } else {
                "Saved $savedCount files to Downloads"
            },
        )
        scheduleAutoDismiss(offerId)
    }

    private fun mergeReportRows(offerId: String, report: BatchReport): List<IncomingFileUi> {
        val current = _incoming.value?.takeIf { it.id == offerId }
        val previous = current?.files?.associateBy { it.index }.orEmpty()
        return report.files.map { item ->
            val mapped = rowFromItem(item)
            val old = previous[mapped.index]
            if (old?.savedUri != null) {
                mapped.copy(savedName = old.savedName, savedUri = old.savedUri)
            } else {
                mapped
            }
        }
    }

    private fun resolveStagedSource(staging: File, item: BatchItemInfo): File {
        val reported = item.destinationPath
            ?.takeIf { it.isNotBlank() }
            ?.let(::File)
        if (reported != null && reported.isFile && isInside(staging, reported)) {
            return reported
        }

        var candidate = staging
        item.relativePath.replace('\\', '/').split('/').forEach { segment ->
            if (segment.isBlank() || segment == "." || segment == ".." || segment.contains(':')) {
                throw IOException("The sender used an unsafe file path")
            }
            candidate = File(candidate, segment)
        }
        if (!candidate.isFile) {
            throw IOException("The received file is no longer available")
        }
        return candidate
    }

    private fun isInside(root: File, candidate: File): Boolean {
        val rootPath = runCatching { root.canonicalPath }.getOrNull() ?: return false
        val candidatePath = runCatching { candidate.canonicalPath }.getOrNull() ?: return false
        return candidatePath == rootPath || candidatePath.startsWith(rootPath + File.separator)
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
        session: BatchTransferSession?,
        error: Throwable,
    ): Boolean {
        consentJob?.cancel()
        consentJob = null

        val cancelled = error is FfiException.Cancelled
        val current = _incoming.value
        // The event pump can lag behind `run`, so consult the session state
        // before treating an authenticated peer's failure as port-scan noise.
        val authenticated = runCatching { session?.peerFingerprint() != null }
            .getOrDefault(false)
        val hadOffer = announced.get() || current?.id == offerId || authenticated

        if (declinedOfferId == offerId) {
            synchronized(lock) {
                if (_incoming.value?.id == offerId) _incoming.value = null
            }
            return false
        }

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

        markFailed(friendlyError(error))
        notify("Incoming transfer failed")
        scheduleAutoDismiss(offerId)
        return false
    }

    private fun markFailed(message: String) {
        _incoming.update { current ->
            if (current == null || current.phase.isTerminal) {
                current
            } else {
                current.copy(
                    phase = IncomingPhase.FAILED,
                    error = message,
                    etaMillis = null,
                    sas = null,
                    files = current.files.map { row ->
                        if (row.phase.isTerminal) {
                            row
                        } else {
                            row.copy(
                                phase = IncomingFilePhase.FAILED,
                                error = row.error ?: message,
                            )
                        }
                    },
                )
            }
        }
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
                    (current.phase == IncomingPhase.SAVED ||
                        current.phase == IncomingPhase.VERIFIED ||
                        current.phase == IncomingPhase.FAILED)
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
            stagingDirectory.listFiles()?.forEach { file -> file.deleteRecursively() }
        }
    }

    private fun deleteStaging(staging: File) {
        runCatching { staging.deleteRecursively() }
    }

    private fun batchTitle(rows: List<IncomingFileUi>): String = when {
        rows.isEmpty() -> "Incoming files"
        rows.size == 1 -> rows.first().name
        else -> "${rows.size} files"
    }

    private fun friendlyError(error: Throwable?): String = when (error) {
        null -> "The transfer failed"
        is StoragePermissionRequiredException ->
            "Allow storage access to save files to Downloads"
        is FfiException.Timeout -> error.message.ifBlank { "The transfer timed out" }
        is FfiException.PeerClosed -> "The other device disconnected"
        is FfiException.PeerAborted -> "The other device cancelled the transfer"
        is FfiException.ChecksumMismatch -> "A checksum did not match"
        is FfiException.Crypto -> error.message.ifBlank { "Secure connection failed" }
        is FfiException.IdentityMismatch ->
            "The other device presented a different security key"
        is FfiException.VerificationRejected -> "Verification was declined"
        is FfiException.AlreadyFinished -> "This verification is no longer available"
        is FfiException.Io -> error.message.ifBlank { "A network error interrupted the transfer" }
        is FfiException.Protocol -> "The other device sent invalid data"
        is FfiException.InvalidArgument -> error.message.ifBlank { "The transfer could not start" }
        is UnsatisfiedLinkError -> "The native core is missing for this device's ABI"
        is IOException -> error.message ?: "A file error interrupted the transfer"
        else -> error.message?.takeIf { it.isNotBlank() } ?: error.javaClass.simpleName
    }

    private fun friendlyError(event: BatchEvent.Failed): String =
        event.message.ifBlank { event.kind.ifBlank { "The transfer failed" } }

    companion object {
        private const val TAG = "InboundReceiver"

        /** Keep in sync with the Rust discovery default and the Windows peer. */
        const val TRANSFER_PORT = 42_517

        private const val STAGING_DIRECTORY = "incoming"
        private const val ACCEPT_TIMEOUT_MS = 120_000L
        private const val ACK_EVERY_BYTES = 4L * 1024L * 1024L
        private const val PROGRESS_INTERVAL_MS = 100L
        private const val RESPONSE_TIMEOUT_MS = 90_000L
        private const val CONSENT_TIMEOUT_MS = 120_000L
        private const val RETRY_FAST_MS = 500L
        private const val RETRY_SLOW_MS = 5_000L
        private const val TERMINAL_CARD_VISIBLE_MS = 8_000L
        private const val WAKE_LOCK_TAG = "AirBridge:inbound"
        private const val WAKE_LOCK_TIMEOUT_MS = 15L * 60L * 1000L
    }
}
