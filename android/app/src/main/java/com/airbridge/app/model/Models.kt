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

/**
 * A document the user selected or dropped onto the radar.
 *
 * [relativePath] is the `/`-separated path carried in the batch manifest. A
 * folder selection preserves the folder prefix (`album/one.jpg`); single files
 * use their own name. It is never the timestamped cache path.
 */
data class SelectedFile(
    val uriString: String,
    val name: String,
    val size: Long,
    val mimeType: String?,
    val relativePath: String = name,
) {
    val key: String get() = uriString
    val folder: String? get() = relativePath.substringBeforeLast('/', "").ifBlank { null }
    val displayPath: String get() = relativePath.ifBlank { name }
}

enum class ShareDirection { SENDING, RECEIVING }

enum class TransferPhase {
    /** Staging the selected documents into private storage. */
    PREPARING,
    CONNECTING,
    VERIFYING,
    RUNNING,
    CANCELLING,
    COMPLETED,
    FAILED,
    CANCELLED;

    val isTerminal: Boolean get() = this == COMPLETED || this == FAILED || this == CANCELLED
    val isActive: Boolean get() = !isTerminal
}

/** State of one file inside an outgoing batch. */
enum class TransferFilePhase {
    PENDING,
    ACTIVE,
    COMPLETED,
    FAILED,
    CANCELLED;

    val isTerminal: Boolean get() = this == COMPLETED || this == FAILED || this == CANCELLED
}

/** One child row under the batch transfer card. */
data class TransferFileUi(
    val index: Int,
    val relativePath: String,
    val size: Long,
    val bytesTransferred: Long = 0L,
    val phase: TransferFilePhase = TransferFilePhase.PENDING,
    val error: String? = null,
) {
    val name: String get() = relativePath.substringAfterLast('/')
    val folder: String? get() = relativePath.substringBeforeLast('/', "").ifBlank { null }
    val progress: Float?
        get() = size.takeIf { it > 0L }
            ?.let { (bytesTransferred.toFloat() / it.toFloat()).coerceIn(0f, 1f) }
    val percent: Int? get() = progress?.let { Math.round(it * 100f) }
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
    /** Per-file rows, in manifest order. */
    val files: List<TransferFileUi> = emptyList(),
    /** Linear estimate supplied by the Rust queue. */
    val etaMillis: Long? = null,
    /** Sender's stable batch id, used when retrying the same manifest. */
    val batchId: ULong? = null,
    /** Six-digit code to compare while [phase] is [TransferPhase.VERIFYING]. */
    val sas: String? = null,
    /** Fingerprint the peer presented once its handshake completed. */
    val peerFingerprint: String? = null,
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

    val isBatch: Boolean get() = files.size > 1

    val completedFiles: Int get() = files.count { it.phase == TransferFilePhase.COMPLETED }

    val failedFiles: Int get() = files.count { it.phase == TransferFilePhase.FAILED }

    /** Files still waiting or in flight. */
    val remainingFiles: Int
        get() = files.count {
            it.phase == TransferFilePhase.PENDING || it.phase == TransferFilePhase.ACTIVE
        }

    val activeFile: TransferFileUi?
        get() = files.firstOrNull { it.phase == TransferFilePhase.ACTIVE }

    val countsText: String
        get() = if (isBatch) {
            "$completedFiles of ${files.size} files"
        } else {
            formatBytes(totalBytes ?: bytesTransferred)
        }

    val etaText: String? get() = formatEta(etaMillis)
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
    /**
     * The handshake finished and the user must compare the SAS with the
     * other device. No payload bytes have been accepted yet.
     */
    VERIFYING,

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

    /** A pairing-only connection finished; no file was involved. */
    VERIFIED,

    /** The transfer or the save failed. */
    FAILED,

    /** The user declined the transfer. */
    DECLINED;

    val isTerminal: Boolean
        get() = this == SAVED || this == VERIFIED || this == FAILED || this == DECLINED
    val isAwaitingConsent: Boolean get() = this == VERIFYING || this == WAITING || this == READY
}

/** State of one file inside an inbound batch. */
enum class IncomingFilePhase {
    PENDING,
    ACTIVE,
    COMPLETED,
    FAILED,
    CANCELLED;

    val isTerminal: Boolean get() = this == COMPLETED || this == FAILED || this == CANCELLED
}

/** One child row under the inbound transfer card. */
data class IncomingFileUi(
    val index: Int,
    val relativePath: String,
    val size: Long,
    val bytesTransferred: Long = 0L,
    val phase: IncomingFilePhase = IncomingFilePhase.PENDING,
    val savedName: String? = null,
    val savedUri: String? = null,
    val error: String? = null,
) {
    val name: String get() = relativePath.substringAfterLast('/')
    val folder: String? get() = relativePath.substringBeforeLast('/', "").ifBlank { null }
    val displayName: String get() = savedName ?: name
    val progress: Float?
        get() = size.takeIf { it > 0L }
            ?.let { (bytesTransferred.toFloat() / it.toFloat()).coerceIn(0f, 1f) }
}

/**
 * Everything the incoming-transfer card needs.
 *
 * A batch manifest arrives before the first payload byte, so [files] is
 * populated as soon as the offer is known. [sas] and [peerFingerprint] are
 * available as soon as the handshake completes. Nothing appears in Downloads
 * until the user accepts and every file is verified.
 */
data class IncomingTransferUi(
    /** Locally unique id for this offer, used to ignore stale events. */
    val id: String,
    val fileName: String = "Incoming files",
    val bytesTransferred: Long = 0L,
    val totalBytes: Long? = null,
    val phase: IncomingPhase = IncomingPhase.WAITING,
    val error: String? = null,
    val savedName: String? = null,
    val savedUri: String? = null,
    /** Per-file rows, in manifest order. */
    val files: List<IncomingFileUi> = emptyList(),
    /** Linear estimate supplied by the Rust queue. */
    val etaMillis: Long? = null,
    val batchId: ULong? = null,
    /** Six-digit code to compare before the payload is accepted. */
    val sas: String? = null,
    /** Authenticated peer id, delivered only after the handshake. */
    val peerIdHex: String? = null,
    /** Fingerprint the peer was authenticated against. */
    val peerFingerprint: String? = null,
    /** True after "Codes match" until the FFI releases the channel. */
    val verificationSent: Boolean = false,
    /** True when this connection only verified the device and moved no file. */
    val pairingOnly: Boolean = false,
) {
    val progress: Float?
        get() = totalBytes
            ?.takeIf { it > 0L }
            ?.let { (bytesTransferred.toFloat() / it.toFloat()).coerceIn(0f, 1f) }

    val percent: Int? get() = progress?.let { Math.round(it * 100f) }

    val isBatch: Boolean get() = files.size > 1

    val completedFiles: Int get() = files.count { it.phase == IncomingFilePhase.COMPLETED }

    val failedFiles: Int get() = files.count { it.phase == IncomingFilePhase.FAILED }

    val remainingFiles: Int
        get() = files.count {
            it.phase == IncomingFilePhase.PENDING || it.phase == IncomingFilePhase.ACTIVE
        }

    val activeFile: IncomingFileUi?
        get() = files.firstOrNull { it.phase == IncomingFilePhase.ACTIVE }

    val etaText: String? get() = formatEta(etaMillis)

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

/**
 * Formats the Rust queue's linear remaining-time estimate.
 *
 * The estimate can jump while a new file starts, so this is deliberately shown
 * as an approximation ("~12 sec left") rather than a countdown.
 */
fun formatEta(millis: Long?): String? {
    if (millis == null || millis <= 0L) return null
    val seconds = (millis + 999L) / 1_000L
    val text = when {
        seconds < 60L -> "$seconds sec"
        seconds < 3_600L -> {
            val minutes = seconds / 60L
            val rest = seconds % 60L
            if (rest >= 30L) "${minutes + 1L} min" else "$minutes min"
        }
        seconds < 86_400L -> {
            val hours = seconds / 3_600L
            val minutes = (seconds % 3_600L) / 60L
            if (minutes == 0L) "$hours hr" else "$hours hr $minutes min"
        }
        else -> {
            val days = seconds / 86_400L
            val hours = (seconds % 86_400L) / 3_600L
            if (hours == 0L) "$days d" else "$days d $hours hr"
        }
    }
    return "~$text left"
}

/** Human-readable summary of the remaining files in a batch. */
fun formatRemaining(files: Int): String = if (files == 1) "1 file left" else "$files files left"
