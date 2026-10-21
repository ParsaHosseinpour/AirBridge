package com.airbridge.app.core

import android.annotation.SuppressLint
import android.content.Context
import android.net.Uri
import android.os.PowerManager
import com.airbridge.app.model.DiscoveryUiState
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.app.model.NearbyPeer
import com.airbridge.app.model.SelectedFile
import com.airbridge.app.model.ShareDirection
import com.airbridge.app.model.TransferPhase
import com.airbridge.app.model.TransferUi
import com.airbridge.core.DiscoveryConfig
import com.airbridge.core.DiscoveryEvent
import com.airbridge.core.FfiException
import com.airbridge.core.FileTransferSession
import com.airbridge.core.PairingChallenge
import com.airbridge.core.PairingResult
import com.airbridge.core.PairingSession
import com.airbridge.core.PeerDiscoveryEngine
import com.airbridge.core.PeerIdentity
import com.airbridge.core.PeerInfo
import com.airbridge.core.SendOptions
import com.airbridge.core.SubnetPolicy
import com.airbridge.core.TransferEvent
import com.airbridge.core.events
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import java.io.File
import java.io.IOException
import java.util.Locale
import java.util.concurrent.atomic.AtomicBoolean

/** Messages the UI shows in a snackbar. */
sealed interface ControllerEvent {
    data class Message(val text: String) : ControllerEvent
}

/**
 * Owns the native discovery engine, the Android BLE backend and every transfer
 * session. It is created by [AirBridgeService] and survives configuration
 * changes; the ViewModel only observes its flows.
 */
class AirBridgeController(
    context: Context,
    private val scope: CoroutineScope,
    private val preferences: AppPreferences,
) {
    private val appContext = context.applicationContext
    private val engineMutex = Mutex()
    private val pairingMutex = Mutex()
    private val started = AtomicBoolean(false)

    private val _discovery = MutableStateFlow(DiscoveryUiState(starting = true))
    val discovery: StateFlow<DiscoveryUiState> = _discovery.asStateFlow()

    private val _peers = MutableStateFlow<List<NearbyPeer>>(emptyList())
    val peers: StateFlow<List<NearbyPeer>> = _peers.asStateFlow()

    private val _transfer = MutableStateFlow<TransferUi?>(null)
    val transfer: StateFlow<TransferUi?> = _transfer.asStateFlow()

    private val _events = MutableSharedFlow<ControllerEvent>(extraBufferCapacity = 16)
    val events: SharedFlow<ControllerEvent> = _events.asSharedFlow()

    /**
     * Owns the Rust receive session bound to the transfer port. It lives for
     * the whole service lifetime, independently of discovery, and is the
     * endpoint the Windows app connects to.
     */
    private val inbound = InboundReceiver(
        appContext,
        scope,
        preferences,
        notify = { message -> emit(message) },
        onPeerVerified = ::rememberVerifiedPeer,
    )

    val incoming: StateFlow<IncomingTransferUi?> = inbound.incoming

    private var engine: PeerDiscoveryEngine? = null
    private var bleBackend: AndroidBleBackend? = null
    private var eventJob: Job? = null
    private var refreshJob: Job? = null
    private var transferJob: Job? = null
    private var activeSession: FileTransferSession? = null
    private var cancelRequested = false
    private var wakeLock: PowerManager.WakeLock? = null

    /** Lazily opened from the persisted PKCS#8 bytes and kept for the service. */
    private var identityRef: PeerIdentity? = null
    private var pairingSession: PairingSession? = null
    private var pairingPeerId: String? = null

    // ---------------------------------------------------------------- lifecycle

    fun start() {
        if (!started.compareAndSet(false, true)) return
        // The receive listener is independent of discovery: inbound transfers
        // must work even when mDNS/BLE are unavailable.
        inbound.start()
        scope.launch(Dispatchers.IO) {
            engineMutex.withLock { startEngineLocked() }
        }
    }

    fun restartDiscovery() {
        if (!started.get()) {
            start()
            return
        }
        inbound.start()
        scope.launch(Dispatchers.IO) {
            engineMutex.withLock {
                stopEngineLocked()
                _discovery.update { it.copy(starting = true, error = null) }
                delay(300)
                startEngineLocked()
            }
        }
    }

    /** Stops discovery and transfers without tearing down the service. */
    fun stopSharing() {
        cancelRequested = true
        runCatching { activeSession?.cancel() }
        inbound.stop()
        scope.launch(Dispatchers.IO) {
            engineMutex.withLock { stopEngineLocked() }
        }
    }

    fun updateDisplayName(name: String) {
        preferences.displayName = name
        restartDiscovery()
    }

    fun shutdown() {
        started.set(false)
        eventJob?.cancel()
        refreshJob?.cancel()
        transferJob?.cancel()
        cancelRequested = true
        runCatching { activeSession?.cancel() }
        inbound.stop()
        runCatching { pairingSession?.close() }
        pairingSession = null
        pairingPeerId = null
        runCatching { identityRef?.close() }
        identityRef = null
        runCatching { engine?.stop() }
        runCatching { engine?.close() }
        engine = null
        bleBackend = null
        releaseWakeLock()
    }

    // ---------------------------------------------------------------- discovery

    private fun startEngineLocked() {
        if (engine != null) return
        val native = NativeCore.status()
        if (!native.available) {
            _discovery.value = DiscoveryUiState(
                starting = false,
                running = false,
                peerIdHex = preferences.peerIdHex,
                error = native.message,
            )
            return
        }

        _discovery.update {
            it.copy(starting = true, running = false, error = null, tiers = emptyMap(), tierErrors = emptyMap())
        }

        try {
            val config = DiscoveryConfig(
                peerIdHex = preferences.peerIdHex,
                peerName = preferences.displayName,
                listenPort = LISTEN_PORT,
                enableBle = true,
                enableMdns = true,
                subnetPolicy = SubnetPolicy.FALLBACK,
            )
            val created = PeerDiscoveryEngine(config)
            val backend = AndroidBleBackend(appContext)
            engine = created
            bleBackend = backend
            created.start(backend)
            created.peerIdHex()?.let(preferences::rememberPeerId)

            _discovery.update {
                it.copy(
                    starting = false,
                    running = true,
                    peerIdHex = preferences.peerIdHex,
                )
            }
            eventJob = scope.launch(Dispatchers.IO) { observeEvents(created) }
            refreshJob = scope.launch(Dispatchers.IO) { refreshLoop(created) }
            updatePeers(created.peers())
        } catch (t: Throwable) {
            _discovery.update {
                it.copy(starting = false, running = false, error = describe(t))
            }
            runCatching { engine?.stop() }
            runCatching { engine?.close() }
            engine = null
            bleBackend = null
        }
    }

    private fun stopEngineLocked() {
        eventJob?.cancel()
        eventJob = null
        refreshJob?.cancel()
        refreshJob = null
        val current = engine
        engine = null
        bleBackend = null
        if (current != null) {
            runCatching { current.stop() }
            runCatching { current.close() }
        }
        _peers.value = emptyList()
        _discovery.update {
            it.copy(starting = false, running = false, tiers = emptyMap())
        }
    }

    private suspend fun observeEvents(current: PeerDiscoveryEngine) {
        try {
            current.events().collect { event ->
                when (event) {
                    is DiscoveryEvent.PeerFound -> upsertPeer(event.peer)
                    is DiscoveryEvent.PeerUpdated -> upsertPeer(event.peer)
                    is DiscoveryEvent.PeerLost -> _peers.update { list ->
                        list.filterNot { it.idHex == event.idHex }
                    }
                    is DiscoveryEvent.Lagged -> runCatching { updatePeers(current.peers()) }
                    is DiscoveryEvent.TierStateChanged -> _discovery.update {
                        it.copy(tiers = it.tiers + (event.tier to event.state))
                    }
                    is DiscoveryEvent.TierError -> _discovery.update {
                        it.copy(tierErrors = it.tierErrors + (event.tier to event.message))
                    }
                    is DiscoveryEvent.SubnetScanStarted -> Unit
                    is DiscoveryEvent.SubnetScanFinished -> Unit
                    DiscoveryEvent.Shutdown -> _discovery.update {
                        it.copy(starting = false, running = false)
                    }
                }
            }
        } catch (c: CancellationException) {
            throw c
        } catch (t: Throwable) {
            if (started.get()) {
                _discovery.update {
                    it.copy(starting = false, running = false, error = describe(t))
                }
            }
        }
    }

    private suspend fun refreshLoop(current: PeerDiscoveryEngine) {
        while (currentCoroutineContext().isActive) {
            delay(PEER_REFRESH_MS)
            try {
                updatePeers(current.peers())
            } catch (c: CancellationException) {
                throw c
            } catch (_: Throwable) {
                // The authoritative snapshot is re-read on the next tick.
            }
        }
    }

    private fun upsertPeer(peer: PeerInfo) {
        val mapped = peer.toNearby()
        _peers.update { list ->
            val without = list.filterNot { it.idHex == mapped.idHex }
            (without + mapped).sortedBy { it.idHex }
        }
    }

    private fun updatePeers(peers: List<PeerInfo>) {
        _peers.value = peers.map { it.toNearby() }.sortedBy { it.idHex }
    }

    private fun PeerInfo.toNearby(): NearbyPeer {
        val cleanName = name?.trim()?.takeIf { it.isNotEmpty() }
            ?: "Device ${idHex.take(4).uppercase(Locale.US)}"
        return NearbyPeer(
            idHex = idHex,
            name = cleanName,
            addresses = addresses,
            isTrusted = preferences.isTrusted(idHex),
            lastSeenMillis = System.currentTimeMillis(),
        )
    }

    // ---------------------------------------------------------------- pairing

    /**
     * Connects to [peer], runs the P-256 handshake and returns the real SAS
     * plus the peer's identity fingerprint.
     */
    suspend fun createChallenge(peer: NearbyPeer): PairingChallenge = pairingMutex.withLock {
        val address = peer.primaryAddress
            ?: throw IOException("${peer.name} has no reachable address yet")
        // A previous half-open attempt cannot coexist with a new one.
        pairingSession?.let { previous ->
            pairingSession = null
            pairingPeerId = null
            runCatching { previous.reject() }
            runCatching { previous.close() }
        }
        val session = PairingSession(
            address,
            identity(),
            preferences.peerIdHex,
            PAIRING_TIMEOUT_MS.toULong(),
        )
        try {
            val challenge = session.start()
            pairingSession = session
            pairingPeerId = peer.idHex
            challenge
        } catch (t: Throwable) {
            runCatching { session.close() }
            throw t
        }
    }

    /**
     * Releases the verified pairing channel and persists trust.
     *
     * Returns `null` when there is no pending challenge or the peer claims a
     * different id than the device the user asked to verify; trust is never
     * written in that case.
     */
    suspend fun approveChallenge(): PairingResult? = pairingMutex.withLock {
        val session = pairingSession ?: return@withLock null
        val expectedPeerId = pairingPeerId
        pairingSession = null
        pairingPeerId = null
        val result = try {
            session.confirm()
        } catch (c: CancellationException) {
            throw c
        } catch (t: Throwable) {
            emit(describe(t))
            return@withLock null
        } finally {
            runCatching { session.close() }
        }
        if (expectedPeerId == null || !result.peerId.equals(expectedPeerId, ignoreCase = true)) {
            emit("The device answered with a different identity")
            return@withLock null
        }
        // The discovered id is the handle the peer list uses; the claimed id
        // was just proven equal to it, so bind trust under that handle.
        rememberVerifiedPeer(expectedPeerId, result.peerFingerprint)
        result
    }

    /** Drops a pending pairing attempt without creating trust. */
    fun cancelPairing() {
        val session = pairingSession ?: return
        pairingSession = null
        pairingPeerId = null
        scope.launch(Dispatchers.IO) {
            runCatching { session.reject() }
            runCatching { session.close() }
        }
    }

    private fun identity(): PeerIdentity = identityRef ?: PeerIdentity
        .fromPkcs8(preferences.identityPkcs8)
        .also { identityRef = it }

    /**
     * Records an authenticated id-to-fingerprint binding and marks it trusted.
     * Only called for a peer whose SAS the user just confirmed (or whose
     * fingerprint was already trusted and auto-confirmed by the FFI).
     */
    private fun rememberVerifiedPeer(peerId: String, fingerprint: String) {
        preferences.trust(peerId, fingerprint)
        _peers.update { list ->
            list.map { peer ->
                if (peer.idHex.equals(peerId, ignoreCase = true)) {
                    peer.copy(isTrusted = true)
                } else {
                    peer
                }
            }
        }
    }

    // ---------------------------------------------------------------- transfers

    fun send(peer: NearbyPeer, files: List<SelectedFile>): Boolean {
        if (files.isEmpty()) return false
        if (transferJob?.isActive == true) {
            emit("Wait for the current transfer to finish")
            return false
        }
        val address = peer.primaryAddress
        if (address == null) {
            emit("${peer.name} has no reachable address yet")
            return false
        }
        cancelRequested = false
        transferJob = scope.launch(Dispatchers.IO) { runSend(peer, address, files) }
        return true
    }

    fun cancelActiveTransfer() {
        cancelRequested = true
        runCatching { activeSession?.cancel() }
        _transfer.update { current ->
            if (current == null || current.phase.isTerminal) {
                current
            } else {
                current.copy(phase = TransferPhase.CANCELLING)
            }
        }
    }

    /** Approves the SAS shown while an outgoing transfer is [TransferPhase.VERIFYING]. */
    fun confirmTransferVerification() {
        val session = activeSession ?: return
        runCatching { session.confirmPeer() }
            .onFailure { emit(describe(it)) }
    }

    /** Declines the SAS shown while an outgoing transfer is [TransferPhase.VERIFYING]. */
    fun rejectTransferVerification() {
        val session = activeSession ?: return
        runCatching { session.rejectPeer() }
            .onFailure { emit(describe(it)) }
    }

    fun dismissTransfer() {
        if (transferJob?.isActive == true) return
        if (_transfer.value?.phase?.isTerminal == true) {
            _transfer.value = null
        }
    }

    /** Approves the pending inbound offer (or retries saving a verified one). */
    fun acceptIncoming() {
        inbound.accept()
    }

    /** Declines a pending offer, or cancels a receive already in flight. */
    fun declineIncoming() {
        inbound.decline()
    }

    /** Clears a completed/failed incoming card once the user has seen it. */
    fun dismissIncoming() {
        inbound.dismiss()
    }

    private suspend fun runSend(peer: NearbyPeer, address: String, files: List<SelectedFile>) {
        acquireWakeLock()
        try {
            for ((index, file) in files.withIndex()) {
                if (cancelRequested) break
                _transfer.value = TransferUi(
                    direction = ShareDirection.SENDING,
                    peerIdHex = peer.idHex,
                    peerName = peer.name,
                    fileName = file.name,
                    totalBytes = file.size.takeIf { it > 0L },
                    phase = TransferPhase.CONNECTING,
                    queueIndex = index + 1,
                    queueSize = files.size,
                )

                val staged = try {
                    stageFile(file)
                } catch (c: CancellationException) {
                    throw c
                } catch (t: Throwable) {
                    _transfer.update { it?.copy(phase = TransferPhase.FAILED, error = describeFileError(file, t)) }
                    break
                }

                try {
                    if (cancelRequested) break
                    var connectAttempt = 0
                    while (!cancelRequested) {
                        val options = try {
                            sendOptions(peer)
                        } catch (t: Throwable) {
                            _transfer.update { it?.copy(phase = TransferPhase.FAILED, error = describe(t)) }
                            break
                        }
                        val session = try {
                            FileTransferSession.send(address, staged.absolutePath, options)
                        } catch (c: CancellationException) {
                            throw c
                        } catch (t: Throwable) {
                            _transfer.update { it?.copy(phase = TransferPhase.FAILED, error = describe(t)) }
                            break
                        }

                        activeSession = session
                        var retry = false
                        try {
                            coroutineScope {
                                val collector = launch {
                                    session.events().collect { event -> onTransferEvent(event) }
                                }
                                try {
                                    val report = session.run()
                                    if (!cancelRequested) {
                                        _transfer.update { current ->
                                            if (current == null) null else current.copy(
                                                phase = TransferPhase.COMPLETED,
                                                bytesTransferred = report.totalBytes.toLong(),
                                                totalBytes = report.totalBytes.toLong()
                                                    .takeIf { it > 0L } ?: current.totalBytes,
                                            )
                                        }
                                    }
                                } catch (c: CancellationException) {
                                    throw c
                                } catch (t: Throwable) {
                                    val cancelled = cancelRequested || t is FfiException.Cancelled
                                    // Pairing and transfers share one listener
                                    // port. Give a receiver that is re-arming
                                    // after a pairing-only connection a few
                                    // chances before failing the card. Never
                                    // retry once a code was shown or the peer
                                    // identity was already authenticated.
                                    retry = !cancelled &&
                                        t is FfiException.Io &&
                                        connectAttempt < MAX_CONNECT_RETRIES &&
                                        session.peerFingerprint() == null &&
                                        _transfer.value?.sas == null
                                    if (!retry) {
                                        _transfer.update { current ->
                                            if (current == null || current.phase.isTerminal) {
                                                current
                                            } else if (cancelled) {
                                                current.copy(phase = TransferPhase.CANCELLED)
                                            } else {
                                                current.copy(phase = TransferPhase.FAILED, error = describe(t))
                                            }
                                        }
                                    }
                                } finally {
                                    collector.cancelAndJoin()
                                }
                            }
                        } finally {
                            runCatching { session.close() }
                            activeSession = null
                        }

                        if (!retry) break
                        connectAttempt++
                        _transfer.update { current ->
                            if (current == null || current.phase.isTerminal) {
                                current
                            } else {
                                current.copy(
                                    phase = TransferPhase.CONNECTING,
                                    sas = null,
                                    error = null,
                                )
                            }
                        }
                        delay(CONNECT_RETRY_DELAY_MS)
                    }
                } finally {
                    staged.delete()
                }

                val phase = _transfer.value?.phase
                if (phase == TransferPhase.FAILED || phase == TransferPhase.CANCELLED || cancelRequested) {
                    break
                }
                delay(300)
            }

            if (cancelRequested && _transfer.value?.phase?.isTerminal != true) {
                _transfer.update { it?.copy(phase = TransferPhase.CANCELLED) }
            }
            _transfer.value?.let { result ->
                when (result.phase) {
                    TransferPhase.COMPLETED -> emit("${result.completedVerb} ${result.fileName}")
                    TransferPhase.FAILED -> emit(result.error ?: "Transfer failed")
                    TransferPhase.CANCELLED -> emit("Transfer cancelled")
                    else -> Unit
                }
            }
        } finally {
            releaseWakeLock()
        }
    }

    private fun onTransferEvent(event: TransferEvent) {
        _transfer.update { current ->
            if (current == null) return@update null
            when (event) {
                is TransferEvent.Started -> current.copy(
                    phase = TransferPhase.RUNNING,
                    totalBytes = event.totalBytes?.toLong() ?: current.totalBytes,
                    sas = null,
                )
                is TransferEvent.VerificationRequired -> current.copy(
                    phase = TransferPhase.VERIFYING,
                    sas = event.sas,
                    peerFingerprint = event.peerFingerprint,
                )
                is TransferEvent.PeerVerified -> {
                    rememberVerifiedPeer(event.peerId, event.peerFingerprint)
                    current.copy(peerFingerprint = event.peerFingerprint)
                }
                is TransferEvent.Progress -> current.copy(
                    phase = if (current.phase == TransferPhase.CONNECTING) {
                        TransferPhase.RUNNING
                    } else {
                        current.phase
                    },
                    bytesTransferred = event.bytesTransferred.toLong(),
                    totalBytes = event.totalBytes?.toLong() ?: current.totalBytes,
                )
                is TransferEvent.Completed -> current.copy(
                    phase = TransferPhase.COMPLETED,
                    bytesTransferred = event.report.totalBytes.toLong(),
                    totalBytes = event.report.totalBytes.toLong().takeIf { it > 0L } ?: current.totalBytes,
                    sas = null,
                )
                is TransferEvent.Failed -> if (current.phase.isTerminal) {
                    current
                } else {
                    current.copy(
                        phase = TransferPhase.FAILED,
                        error = event.message.ifBlank { event.kind },
                        sas = null,
                    )
                }
                TransferEvent.Cancelled -> if (current.phase.isTerminal) {
                    current
                } else {
                    current.copy(phase = TransferPhase.CANCELLED, sas = null)
                }
                is TransferEvent.Lagged -> current
            }
        }
    }

    private fun sendOptions(peer: NearbyPeer): SendOptions {
        val pinned = if (preferences.isTrusted(peer.idHex)) {
            preferences.peerFingerprint(peer.idHex)
        } else {
            null
        }
        return SendOptions(
            identityPkcs8 = preferences.identityPkcs8,
            localPeerId = preferences.peerIdHex,
            trustedPeerFingerprints = preferences.trustedFingerprints().toList(),
            expectedPeerFingerprint = pinned,
        )
    }

    private fun stageFile(file: SelectedFile): File {
        val directory = File(appContext.cacheDir, OUTGOING_DIR)
        if (!directory.exists() && !directory.mkdirs()) {
            throw IOException("Could not create the outgoing cache directory")
        }
        val target = File(directory, "${System.currentTimeMillis()}_${sanitize(file.name)}")
        appContext.contentResolver.openInputStream(Uri.parse(file.uriString))?.use { input ->
            target.outputStream().use { output -> input.copyTo(output, COPY_BUFFER_BYTES) }
        } ?: throw IOException("The document could not be opened")
        if (target.length() <= 0L) {
            target.delete()
            throw IOException("The document is empty")
        }
        return target
    }

    private fun sanitize(name: String): String {
        val cleaned = name.replace(Regex("[^A-Za-z0-9._-]"), "_").takeLast(64)
        return cleaned.ifBlank { "airbridge-file" }
    }

    // ---------------------------------------------------------------- helpers

    private fun acquireWakeLock() {
        releaseWakeLock()
        val manager = appContext.getSystemService(PowerManager::class.java) ?: return
        val lock = manager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "AirBridge:transfer")
        lock.setReferenceCounted(false)
        lock.acquire(WAKELOCK_TIMEOUT_MS)
        wakeLock = lock
    }

    private fun releaseWakeLock() {
        val lock = wakeLock ?: return
        wakeLock = null
        runCatching { if (lock.isHeld) lock.release() }
    }

    private fun emit(text: String) {
        _events.tryEmit(ControllerEvent.Message(text))
    }

    private fun describeFileError(file: SelectedFile, t: Throwable): String = when (t) {
        is SecurityException -> "Access to ${file.name} was denied"
        else -> "Could not read ${file.name}"
    }

    private fun describe(t: Throwable): String = when (t) {
        is FfiException.Discovery -> t.message.ifBlank { "Discovery failed" }
        is FfiException.InvalidArgument -> t.message.ifBlank { "Invalid discovery configuration" }
        is FfiException.NotStarted -> "Discovery is not running"
        is FfiException.AlreadyStarted -> "Discovery is already running"
        is FfiException.AlreadyFinished -> "This verification is no longer available"
        is FfiException.Timeout -> t.message.ifBlank { "The transfer timed out" }
        is FfiException.PeerClosed -> "The other device disconnected"
        is FfiException.PeerAborted -> "The other device cancelled the transfer"
        is FfiException.ChecksumMismatch -> "A checksum did not match"
        is FfiException.Crypto -> t.message.ifBlank { "Secure connection failed" }
        is FfiException.IdentityMismatch ->
            "This device's security key changed. Verify it again before sending."
        is FfiException.VerificationRejected -> "Verification was declined"
        is FfiException.Io -> t.message.ifBlank { "A network error interrupted the transfer" }
        is FfiException.Protocol -> "The other device sent invalid data"
        is UnsatisfiedLinkError -> "The native core is missing for this device's ABI"
        is IOException -> t.message ?: "A file error interrupted the transfer"
        else -> t.message?.takeIf { it.isNotBlank() } ?: t.javaClass.simpleName
    }

    companion object {
        /** Default port advertised by the Rust discovery configuration. */
        val LISTEN_PORT: UShort get() = InboundReceiver.TRANSFER_PORT.toUShort()
        private const val PEER_REFRESH_MS = 2_000L
        private const val WAKELOCK_TIMEOUT_MS = 15L * 60L * 1000L
        private const val COPY_BUFFER_BYTES = 256 * 1024
        private const val OUTGOING_DIR = "outgoing"
        private const val PAIRING_TIMEOUT_MS = 30_000L
        private const val MAX_CONNECT_RETRIES = 4
        private const val CONNECT_RETRY_DELAY_MS = 600L
    }
}
