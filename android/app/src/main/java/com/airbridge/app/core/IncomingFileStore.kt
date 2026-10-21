package com.airbridge.app.core

import android.Manifest
import android.content.ContentValues
import android.content.Context
import android.content.pm.PackageManager
import android.media.MediaScannerConnection
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import java.io.File
import java.io.IOException
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** Where a verified inbound payload ended up. */
data class SavedIncomingFile(
    val uri: String,
    val name: String,
)

/** Raised when a pre-Android-10 device needs the legacy storage permission. */
class StoragePermissionRequiredException :
    IOException("Storage access is required to save files to Downloads")

/**
 * Commits a file the Rust receiver staged in the app cache into the device's
 * public Downloads collection.
 *
 * Why staging? [com.airbridge.core.FileTransferSession] receives into a
 * filesystem path, while MediaStore is a content-provider URI. There is no FD
 * hand-off in the FFI, so the verified staging file is streamed into
 * MediaStore only after the user approves it. Nothing is visible in Downloads
 * before that copy starts.
 */
class IncomingFileStore(context: Context) {
    private val appContext = context.applicationContext

    /** True when the caller may copy into Downloads without a runtime prompt. */
    fun hasStorageAccess(): Boolean =
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q || legacyPermissionGranted()

    /** Legacy public-Downloads writes are only needed on API 26-28. */
    fun requiresStoragePermission(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.Q && !legacyPermissionGranted()

    /**
     * Copies [source] into Downloads/AirBridge and returns the public URI.
     *
     * @throws StoragePermissionRequiredException on API 26-28 when
     *   `WRITE_EXTERNAL_STORAGE` has not been granted.
     */
    fun save(source: File, displayName: String): SavedIncomingFile {
        if (!source.isFile) {
            throw IOException("The received file is no longer available")
        }
        val safeName = sanitize(displayName)
        if (requiresStoragePermission()) throw StoragePermissionRequiredException()

        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            saveWithMediaStore(source, safeName)
        } else {
            saveToLegacyDownloads(source, safeName)
        }
    }

    @RequiresApi(Build.VERSION_CODES.Q)
    private fun saveWithMediaStore(source: File, displayName: String): SavedIncomingFile {
        val resolver = appContext.contentResolver
        val collection = MediaStore.Downloads.getContentUri(MediaStore.VOLUME_EXTERNAL_PRIMARY)
        val values = ContentValues().apply {
            put(MediaStore.MediaColumns.DISPLAY_NAME, displayName)
            put(MediaStore.MediaColumns.MIME_TYPE, IncomingFileKinds.mimeType(displayName))
            put(
                MediaStore.MediaColumns.RELATIVE_PATH,
                "${Environment.DIRECTORY_DOWNLOADS}/$DOWNLOAD_FOLDER",
            )
            put(MediaStore.MediaColumns.IS_PENDING, 1)
        }

        val uri = resolver.insert(collection, values)
            ?: throw IOException("Downloads did not accept a new file")

        try {
            resolver.openOutputStream(uri, "w")?.use { output ->
                source.inputStream().use { input -> input.copyTo(output, COPY_BUFFER_BYTES) }
            } ?: throw IOException("Downloads could not open the destination file")

            val published = ContentValues().apply {
                put(MediaStore.MediaColumns.IS_PENDING, 0)
            }
            resolver.update(uri, published, null, null)
            return SavedIncomingFile(uri.toString(), displayName)
        } catch (t: Throwable) {
            // Do not leave a half-written, pending row behind.
            runCatching { resolver.delete(uri, null, null) }
            throw t
        }
    }

    @Suppress("DEPRECATION")
    private fun saveToLegacyDownloads(source: File, displayName: String): SavedIncomingFile {
        val downloads = Environment.getExternalStoragePublicDirectory(
            Environment.DIRECTORY_DOWNLOADS,
        )
        val directory = File(downloads, DOWNLOAD_FOLDER)
        if (!directory.exists() && !directory.mkdirs()) {
            throw IOException("Could not create the Downloads/AirBridge folder")
        }

        val target = uniqueFile(directory, displayName)
        source.inputStream().use { input ->
            target.outputStream().use { output -> input.copyTo(output, COPY_BUFFER_BYTES) }
        }
        MediaScannerConnection.scanFile(
            appContext,
            arrayOf(target.absolutePath),
            arrayOf(IncomingFileKinds.mimeType(target.name)),
            null,
        )
        return SavedIncomingFile(Uri.fromFile(target).toString(), target.name)
    }

    private fun uniqueFile(directory: File, displayName: String): File {
        val extension = displayName.substringAfterLast('.', "")
        val stem = if (extension.isEmpty()) {
            displayName
        } else {
            displayName.dropLast(extension.length + 1)
        }
        val suffix = if (extension.isEmpty()) "" else ".$extension"

        var candidate = File(directory, "$stem$suffix")
        var index = 2
        while (candidate.exists() && index < MAX_UNIQUE_ATTEMPTS) {
            candidate = File(directory, "$stem ($index)$suffix")
            index++
        }
        return candidate
    }

    private fun legacyPermissionGranted(): Boolean =
        ContextCompat.checkSelfPermission(
            appContext,
            Manifest.permission.WRITE_EXTERNAL_STORAGE,
        ) == PackageManager.PERMISSION_GRANTED

    private fun sanitize(name: String): String {
        val cleaned = name
            .replace(Regex("[\\\\/:*?\"<>|\\u0000-\\u001F]"), "_")
            .trim()
            .take(MAX_NAME_LENGTH)
        return cleaned.ifBlank { "AirBridge received file.bin" }
    }

    companion object {
        const val DOWNLOAD_FOLDER = "AirBridge"
        private const val COPY_BUFFER_BYTES = 256 * 1024
        private const val MAX_NAME_LENGTH = 140
        private const val MAX_UNIQUE_ATTEMPTS = 10_000
    }
}

/** Best-effort extension sniffing for an anonymous AirBridge payload. */
internal object IncomingFileKinds {
    fun sniffExtension(file: File): String {
        val header = ByteArray(512)
        val length = try {
            file.inputStream().use { it.read(header) }
        } catch (_: IOException) {
            return ".bin"
        }
        if (length <= 0) return ".bin"
        val bytes = header.copyOf(length)

        return when {
            bytes.startsWith(0x89, 0x50, 0x4E, 0x47) -> ".png"
            bytes.startsWith(0xFF, 0xD8, 0xFF) -> ".jpg"
            bytes.startsWith(0x47, 0x49, 0x46, 0x38) -> ".gif"
            bytes.startsWith(0x25, 0x50, 0x44, 0x46) -> ".pdf"
            bytes.startsWith(0x50, 0x4B, 0x03, 0x04) ||
                bytes.startsWith(0x50, 0x4B, 0x05, 0x06) -> ".zip"
            bytes.startsWith(0x52, 0x61, 0x72, 0x21) -> ".rar"
            bytes.startsWith(0x37, 0x7A, 0xBC, 0xAF) -> ".7z"
            bytes.startsWith(0x66, 0x4C, 0x61, 0x43) -> ".flac"
            bytes.startsWith(0x4F, 0x67, 0x67, 0x53) -> ".ogg"
            bytes.startsWith(0x1A, 0x45, 0xDF, 0xA3) -> ".mkv"
            bytes.startsWith(0x49, 0x44, 0x33) -> ".mp3"
            bytes.size >= 2 && bytes[0] == 0xFF.toByte() &&
                (bytes[1].toInt() and 0xE0) == 0xE0 -> ".mp3"
            bytes.startsWith(0x52, 0x49, 0x46, 0x46) && bytes.size >= 12 &&
                bytes[8] == 'W'.code.toByte() && bytes[9] == 'A'.code.toByte() &&
                bytes[10] == 'V'.code.toByte() && bytes[11] == 'E'.code.toByte() -> ".wav"
            bytes.size >= 12 && bytes[4] == 'f'.code.toByte() &&
                bytes[5] == 't'.code.toByte() && bytes[6] == 'y'.code.toByte() &&
                bytes[7] == 'p'.code.toByte() -> isoBrandExtension(bytes)
            isProbablyText(bytes) -> ".txt"
            else -> ".bin"
        }
    }

    fun mimeType(fileName: String): String {
        val extension = fileName.substringAfterLast('.', "").lowercase(Locale.US)
        if (extension.isEmpty()) return "application/octet-stream"
        MimeTypeMap.getSingleton().getMimeTypeFromExtension(extension)?.let { return it }
        return when (extension) {
            "png" -> "image/png"
            "jpg", "jpeg" -> "image/jpeg"
            "gif" -> "image/gif"
            "pdf" -> "application/pdf"
            "zip" -> "application/zip"
            "rar" -> "application/vnd.rar"
            "7z" -> "application/x-7z-compressed"
            "flac" -> "audio/flac"
            "ogg" -> "audio/ogg"
            "mkv" -> "video/x-matroska"
            "mp3" -> "audio/mpeg"
            "wav" -> "audio/wav"
            "heic" -> "image/heic"
            "mov" -> "video/quicktime"
            "avif" -> "image/avif"
            "mp4" -> "video/mp4"
            "txt" -> "text/plain"
            else -> "application/octet-stream"
        }
    }

    private fun isoBrandExtension(bytes: ByteArray): String {
        val brand = String(bytes, 8, 4, Charsets.US_ASCII)
        return when (brand) {
            "heic", "heix", "heif", "mif1" -> ".heic"
            "qt  " -> ".mov"
            "avif" -> ".avif"
            else -> ".mp4"
        }
    }

    private fun ByteArray.startsWith(vararg prefix: Int): Boolean {
        if (size < prefix.size) return false
        return prefix.indices.all { this[it] == prefix[it].toByte() }
    }

    private fun isProbablyText(bytes: ByteArray): Boolean {
        if (bytes.size < 8) return false
        var printable = 0
        for (value in bytes) {
            if (value == 0.toByte()) return false
            val unsigned = value.toInt() and 0xFF
            if (unsigned in 0x20..0x7E || unsigned == 0x09 || unsigned == 0x0A ||
                unsigned == 0x0D
            ) {
                printable++
            }
        }
        return printable * 100 / bytes.size >= 95
    }
}

/** Builds the user-facing name for an anonymous inbound payload. */
internal object IncomingNames {
    fun friendly(payloadId: ULong?, timestamp: Long = System.currentTimeMillis(), extension: String): String {
        val stamp = SimpleDateFormat("yyyy-MM-dd HH-mm-ss", Locale.US).format(Date(timestamp))
        val suffix = if (extension.startsWith('.')) extension else ".$extension"
        val id = payloadId?.let { String.format(Locale.US, "%08x", it.toLong() and 0xFFFF_FFFFL) }
            ?: "unknown"
        return "AirBridge $stamp $id$suffix"
    }
}

/**
 * Reads the payload size out of the transport's resume sidecar.
 *
 * The FFI receive event does not carry the `HELLO` size, but the receiver
 * persists `payload_id` and `total_len` in `destination.part.state` after the
 * first acknowledged span. The format is the stable, versioned `ABRS` sidecar
 * documented by `libcore_transport`; a missing or partial file simply means
 * "size not known yet".
 */
internal object InboundResumeHints {
    private const val STATE_LEN = 98
    private const val MAGIC_0 = 'A'.code.toByte()
    private const val MAGIC_1 = 'B'.code.toByte()
    private const val MAGIC_2 = 'R'.code.toByte()
    private const val MAGIC_3 = 'S'.code.toByte()
    private const val VERSION: Byte = 1

    fun totalBytes(destination: File): Long? {
        val stateFile = File(destination.absolutePath + ".part.state")
        if (!stateFile.isFile) return null
        val bytes = runCatching { stateFile.readBytes() }.getOrNull() ?: return null
        if (bytes.size != STATE_LEN) return null
        if (bytes[0] != MAGIC_0 || bytes[1] != MAGIC_1 ||
            bytes[2] != MAGIC_2 || bytes[3] != MAGIC_3
        ) {
            return null
        }
        if (bytes[4] != VERSION) return null

        val buffer = ByteBuffer.wrap(bytes).order(ByteOrder.BIG_ENDIAN)
        // 0..4 magic, 4 version, 5 flags, 6..14 payload id, 14..22 total len.
        val total = buffer.getLong(14)
        return total.takeIf { it > 0L }
    }
}
