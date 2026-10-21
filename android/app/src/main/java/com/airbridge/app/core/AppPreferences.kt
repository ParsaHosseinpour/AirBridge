package com.airbridge.app.core

import android.content.Context
import android.os.Build
import android.util.Base64
import com.airbridge.core.PeerIdentity
import java.security.SecureRandom
import java.util.Locale

/**
 * Small, synchronous preference store for the identity the Rust discovery
 * engine advertises and the peer identities the user has verified.
 *
 * Trust is never keyed by the advertised peer id alone: the id is only a
 * discovery handle. A peer becomes trusted when the user confirms the SAS for
 * its long-lived P-256 key, and the resulting SHA-256 fingerprint is what the
 * Rust handshake checks on later transfers.
 */
class AppPreferences(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    init {
        // Trust used to be a peer-id allow-list. An id is unauthenticated, so
        // those entries are discarded rather than migrated.
        if (prefs.contains(KEY_LEGACY_TRUSTED)) {
            prefs.edit().remove(KEY_LEGACY_TRUSTED).apply()
        }
    }

    /**
     * Stable 32-digit hexadecimal peer id. The Rust side treats this as an
     * opaque handle, but keeping it stable lets peers recognise this device
     * between sessions.
     */
    val peerIdHex: String by lazy {
        val existing = prefs.getString(KEY_PEER_ID, null)
        if (existing != null && isValidPeerId(existing)) {
            existing.lowercase(Locale.US).also { normalized ->
                if (normalized != existing) {
                    prefs.edit().putString(KEY_PEER_ID, normalized).apply()
                }
            }
        } else {
            generatePeerId().also { prefs.edit().putString(KEY_PEER_ID, it).apply() }
        }
    }

    /**
     * This device's long-lived identity key as PKCS#8 DER.
     *
     * The key is generated once and stored Base64-encoded in private
     * preferences. It never leaves the device; only its public half and the
     * SHA-256 fingerprint are exchanged during the handshake.
     */
    val identityPkcs8: ByteArray by lazy { loadOrCreateIdentity() }

    /** SHA-256 fingerprint of [identityPkcs8], 64 lowercase hex characters. */
    val identityFingerprint: String by lazy {
        PeerIdentity.fromPkcs8(identityPkcs8).use { it.fingerprint() }
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
            prefs.edit().putString(KEY_PEER_ID, idHex.lowercase(Locale.US)).apply()
        }
    }

    /**
     * True when [idHex] is currently bound to a fingerprint the user verified.
     * Legacy id-only trust entries are ignored because they prove nothing.
     */
    fun isTrusted(idHex: String): Boolean {
        val fingerprint = peerFingerprint(idHex) ?: return false
        return trustedFingerprints().contains(fingerprint)
    }

    /** Fingerprint this device last bound to [idHex], if any. */
    fun peerFingerprint(idHex: String): String? {
        if (!isValidPeerId(idHex)) return null
        return prefs.getString(KEY_PEER_FINGERPRINT + idHex.lowercase(Locale.US), null)
            ?.takeIf(::isValidFingerprint)
    }

    /** Fingerprints the user has verified on any peer. */
    fun trustedFingerprints(): Set<String> =
        prefs.getStringSet(KEY_TRUSTED_FINGERPRINTS, emptySet())
            ?.filter(::isValidFingerprint)
            ?.toSet()
            ?: emptySet()

    /**
     * Records the id-to-fingerprint mapping learned from an authenticated
     * handshake, without granting trust yet.
     */
    fun bindPeer(idHex: String, fingerprintHex: String) {
        if (!isValidPeerId(idHex) || !isValidFingerprint(fingerprintHex)) return
        val id = idHex.lowercase(Locale.US)
        val fingerprint = fingerprintHex.lowercase(Locale.US)
        val editor = prefs.edit()
        // A fingerprint identifies one key, hence one device. Drop stale
        // mappings that point another id at the same key.
        prefs.all.forEach { (key, value) ->
            if (key.startsWith(KEY_PEER_FINGERPRINT) &&
                key != KEY_PEER_FINGERPRINT + id &&
                value is String &&
                value.equals(fingerprint, ignoreCase = true)
            ) {
                editor.remove(key)
            }
        }
        editor.putString(KEY_PEER_FINGERPRINT + id, fingerprint)
        editor.apply()
    }

    /** Binds [idHex] to [fingerprintHex] and marks the fingerprint trusted. */
    fun trust(idHex: String, fingerprintHex: String) {
        if (!isValidPeerId(idHex) || !isValidFingerprint(fingerprintHex)) return
        val id = idHex.lowercase(Locale.US)
        val fingerprint = fingerprintHex.lowercase(Locale.US)
        val previous = peerFingerprint(id)
        bindPeer(id, fingerprint)

        val updated = trustedFingerprints().toMutableSet().apply { add(fingerprint) }
        if (previous != null && previous != fingerprint &&
            prefs.all.none { (key, value) ->
                key.startsWith(KEY_PEER_FINGERPRINT) &&
                    value is String &&
                    value.equals(previous, ignoreCase = true)
            }
        ) {
            // The user verified a replacement key for this peer, so trust in
            // the superseded key must not survive the identity change.
            updated.remove(previous)
        }
        prefs.edit().putStringSet(KEY_TRUSTED_FINGERPRINTS, updated).apply()
    }

    /** Removes the peer's mapping and revokes trust in its fingerprint. */
    fun untrust(idHex: String) {
        if (!isValidPeerId(idHex)) return
        val id = idHex.lowercase(Locale.US)
        val fingerprint = peerFingerprint(id)
        val editor = prefs.edit().remove(KEY_PEER_FINGERPRINT + id)
        if (fingerprint != null &&
            prefs.all.none { (key, value) ->
                key.startsWith(KEY_PEER_FINGERPRINT) &&
                    key != KEY_PEER_FINGERPRINT + id &&
                    value is String &&
                    value.equals(fingerprint, ignoreCase = true)
            }
        ) {
            val remaining = trustedFingerprints().toMutableSet().apply { remove(fingerprint) }
            editor.putStringSet(KEY_TRUSTED_FINGERPRINTS, remaining)
        }
        editor.apply()
    }

    private fun loadOrCreateIdentity(): ByteArray {
        val stored = prefs.getString(KEY_IDENTITY_PKCS8, null)
        if (stored != null) {
            val decoded = runCatching {
                Base64.decode(stored, Base64.NO_WRAP)
            }.getOrNull()
            if (decoded != null) {
                val valid = runCatching {
                    PeerIdentity.fromPkcs8(decoded).use { it.toPkcs8() }
                }.getOrNull()
                if (valid != null) {
                    return valid
                }
            }
        }
        val generated = PeerIdentity.generate().use { it.toPkcs8() }
        prefs.edit()
            .putString(KEY_IDENTITY_PKCS8, Base64.encodeToString(generated, Base64.NO_WRAP))
            .apply()
        return generated
    }

    private fun defaultName(): String {
        val model = Build.MODEL
            ?.trim()
            ?.takeIf { it.isNotEmpty() }
            ?: "Android device"
        return model.take(MAX_NAME_LENGTH)
    }

    private fun isValidPeerId(candidate: String): Boolean =
        candidate.length == 32 && candidate.all { it.isDigit() || it.lowercaseChar() in 'a'..'f' }

    private fun isValidFingerprint(candidate: String): Boolean =
        candidate.length == FINGERPRINT_LENGTH &&
            candidate.all { it.isDigit() || it.lowercaseChar() in 'a'..'f' }

    private fun generatePeerId(): String {
        val bytes = ByteArray(16)
        SecureRandom().nextBytes(bytes)
        return bytes.joinToString(separator = "") { String.format(Locale.US, "%02x", it.toInt() and 0xFF) }
    }

    companion object {
        private const val PREFS_NAME = "airbridge.preferences"
        private const val KEY_PEER_ID = "peer_id_hex"
        private const val KEY_DISPLAY_NAME = "display_name"
        private const val KEY_IDENTITY_PKCS8 = "identity_pkcs8_base64"
        private const val KEY_TRUSTED_FINGERPRINTS = "trusted_peer_fingerprints"
        private const val KEY_PEER_FINGERPRINT = "peer_fingerprint_"
        private const val KEY_LEGACY_TRUSTED = "trusted_peer_ids"
        private const val FINGERPRINT_LENGTH = 64
        private const val MAX_NAME_LENGTH = 40
    }
}
