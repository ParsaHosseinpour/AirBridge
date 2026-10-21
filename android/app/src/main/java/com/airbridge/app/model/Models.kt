package com.airbridge.app.model

import com.airbridge.core.DiscoveryTier
import com.airbridge.core.TierState
import java.util.Locale

/** A discovered peer, rendered as one bubble on the radar. */
data class NearbyPeer(
    val idHex: String,
    val name: String,
    val addresses: List<String>,
    val isTrusted: Boolean,
    val lastSeenMillis: Long = System.currentTimeMillis(),
) {
    val primaryAddress: String? get() = addresses.firstOrNull()
    val shortId: String get() = idHex.take(8).uppercase(Locale.US)
    val initials: String get() = name.initials()
}

/** A document the user selected or dropped onto the radar. */
data class SelectedFile(
    val uriString: String,
    val name: String,
    val size: Long,
    val mimeType: String?,
) {
    val key: String get() = uriString
}

enum class ShareDirection { SENDING, RECEIVING }

enum class TransferPhase {
    CONNECTING,
    RUNNING,
    CANCELLING,
    COMPLETED,
    FAILED,
    CANCELLED;

    val isTerminal: Boolean get() = this == COMPLETED || this == FAILED || this == CANCELLED
    val isActive: Boolean get() = !isTerminal
}

/** Everything the radar needs to draw the current transfer. */
data class TransferUi(
    val direction: ShareDirection,
    val peerIdHex: String,
    val peerName: String,
    val fileName: String,
    val bytesTransferred: Long = 0L,
    val totalBytes: Long? = null,
    val phase: TransferPhase = TransferPhase.CONNECTING,
    val error: String? = null,
    val queueIndex: Int = 1,
    val queueSize: Int = 1,
) {
    val progress: Float?
        get() = totalBytes
            ?.takeIf { it > 0L }
            ?.let { (bytesTransferred.toFloat() / it.toFloat()).coerceIn(0f, 1f) }

    val percent: Int? get() = progress?.let { Math.round(it * 100f) }

    val verb: String
        get() = if (direction == ShareDirection.SENDING) "Sending" else "Receiving"

    val completedVerb: String
        get() = if (direction == ShareDirection.SENDING) "Sent" else "Received"
}

/**
 * Lifecycle of one inbound transfer.
 *
 * The Rust transport accepts the TCP connection and writes the payload into a
 * private staging file before the UI can ask the user. [WAITING] is therefore
 * "bytes may already be arriving, but nothing is visible in Downloads yet".
 * Nothing leaves the app's cache until [SAVED].
 */
enum class IncomingPhase {
    /** A peer connected. The user has not accepted or declined yet. */
    WAITING,

    /** The user accepted; bytes are still arriving in private staging. */
    RECEIVING,

    /** The payload is verified and waiting for a save/discard decision. */
    READY,

    /** The verified staging file is being copied into Downloads. */
    SAVING,

    /** The file is in the public Downloads collection. */
    SAVED,

    /** The transfer or the save failed. */
    FAILED,

    /** The user declined the transfer. */
    DECLINED;

    val isTerminal: Boolean get() = this == SAVED || this == FAILED || this == DECLINED
    val isAwaitingConsent: Boolean get() = this == WAITING || this == READY
}

/**
 * Everything the incoming-transfer card needs.
 *
 * The AirBridge wire protocol deliberately carries no filename or peer
 * identity. [fileName] is a friendly name generated locally after the payload
 * is verified; until then the UI says "Incoming file".
 */
data class IncomingTransferUi(
    /** Locally unique id for this offer, used to ignore stale events. */
    val id: String,
    val fileName: String = "Incoming file",
    val bytesTransferred: Long = 0L,
    val totalBytes: Long? = null,
    val phase: IncomingPhase = IncomingPhase.WAITING,
    val error: String? = null,
    val savedName: String? = null,
    val savedUri: String? = null,
) {
    /**
     * Progress in `0f..1f` once the payload size is known.
     *
     * The receive FFI reports movement but not the HELLO's total size. The
     * receiver adapter reads the size from the transport's resume sidecar as
     * soon as the first acknowledged span exists, then feeds it here.
     */
    val progress: Float?
        get() = totalBytes
            ?.takeIf { it > 0L }
            ?.let { (bytesTransferred.toFloat() / it.toFloat()).coerceIn(0f, 1f) }

    val percent: Int? get() = progress?.let { Math.round(it * 100f) }

    /** True while the ring should pulse/spin instead of drawing a fraction. */
    val isIndeterminate: Boolean
        get() = phase != IncomingPhase.SAVED && progress == null
}

/** Summary of discovery tiers, shown as status pills under the radar. */
data class DiscoveryUiState(
    val starting: Boolean = false,
    val running: Boolean = false,
    val peerIdHex: String? = null,
    val tiers: Map<DiscoveryTier, TierState> = emptyMap(),
    val tierErrors: Map<DiscoveryTier, String> = emptyMap(),
    val error: String? = null,
) {
    fun stateOf(tier: DiscoveryTier): TierState = tiers[tier] ?: TierState.Stopped
}

fun String.initials(): String {
    val parts = trim().split(Regex("\\s+")).filter { it.isNotBlank() }
    return when {
        parts.isEmpty() -> "?"
        parts.size == 1 -> parts[0].take(2).uppercase(Locale.getDefault())
        else -> (parts[0].take(1) + parts[1].take(1)).uppercase(Locale.getDefault())
    }
}

fun formatBytes(bytes: Long): String {
    if (bytes <= 0L) return "Unknown size"
    if (bytes < 1024L) return "$bytes B"
    val units = arrayOf("KB", "MB", "GB", "TB")
    var value = bytes.toDouble() / 1024.0
    var unit = 0
    while (value >= 1024.0 && unit < units.lastIndex) {
        value /= 1024.0
        unit++
    }
    val pattern = if (value >= 10.0) "%.0f %s" else "%.1f %s"
    return String.format(Locale.US, pattern, value, units[unit])
}

fun formatCount(count: Int): String = if (count == 1) "1 file" else "$count files"
