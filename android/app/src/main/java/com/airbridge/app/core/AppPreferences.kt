package com.airbridge.app.core

import android.content.Context
import android.os.Build
import java.security.SecureRandom
import java.util.Locale

/**
 * Small, synchronous preference store for the identity the Rust discovery
 * engine advertises and the set of peers the user has verified.
 */
class AppPreferences(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    /**
     * Stable 32-digit hexadecimal peer id. The Rust side treats this as an
     * opaque handle, but keeping it stable lets peers recognise this device
     * between sessions.
     */
    val peerIdHex: String by lazy {
        val existing = prefs.getString(KEY_PEER_ID, null)
        if (existing != null && isValidPeerId(existing)) {
            existing
        } else {
            generatePeerId().also { prefs.edit().putString(KEY_PEER_ID, it).apply() }
        }
    }

    var displayName: String
        get() = prefs.getString(KEY_DISPLAY_NAME, null)
            ?.trim()
            ?.takeIf { it.isNotEmpty() }
            ?: defaultName()
        set(value) {
            val trimmed = value.trim().take(MAX_NAME_LENGTH)
            if (trimmed.isNotEmpty()) {
                prefs.edit().putString(KEY_DISPLAY_NAME, trimmed).apply()
            }
        }

    /** Persist the id the native engine generated, if we did not supply one. */
    fun rememberPeerId(idHex: String) {
        if (isValidPeerId(idHex)) {
            prefs.edit().putString(KEY_PEER_ID, idHex).apply()
        }
    }

    fun isTrusted(idHex: String): Boolean = trustedIds().contains(idHex)

    fun trust(idHex: String) {
        if (!isValidPeerId(idHex)) return
        val updated = trustedIds().toMutableSet().apply { add(idHex) }
        prefs.edit().putStringSet(KEY_TRUSTED, updated).apply()
    }

    fun untrust(idHex: String) {
        val updated = trustedIds().toMutableSet().apply { remove(idHex) }
        prefs.edit().putStringSet(KEY_TRUSTED, updated).apply()
    }

    fun trustedIds(): Set<String> =
        prefs.getStringSet(KEY_TRUSTED, emptySet())?.toSet() ?: emptySet()

    private fun defaultName(): String {
        val model = Build.MODEL
            ?.trim()
            ?.takeIf { it.isNotEmpty() }
            ?: "Android device"
        return model.take(MAX_NAME_LENGTH)
    }

    private fun isValidPeerId(candidate: String): Boolean =
        candidate.length == 32 && candidate.all { it.isDigit() || it.lowercaseChar() in 'a'..'f' }

    private fun generatePeerId(): String {
        val bytes = ByteArray(16)
        SecureRandom().nextBytes(bytes)
        return bytes.joinToString(separator = "") { String.format(Locale.US, "%02x", it.toInt() and 0xFF) }
    }

    companion object {
        private const val PREFS_NAME = "airbridge.preferences"
        private const val KEY_PEER_ID = "peer_id_hex"
        private const val KEY_DISPLAY_NAME = "display_name"
        private const val KEY_TRUSTED = "trusted_peer_ids"
        private const val MAX_NAME_LENGTH = 40
    }
}
