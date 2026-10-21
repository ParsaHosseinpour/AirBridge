package com.airbridge.app.core

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
import java.security.SecureRandom
import java.util.Locale

/** A short authentication string shown on both devices during pairing. */
data class SasChallenge(
    val peerIdHex: String,
    val code: String,
    val createdAtMillis: Long,
)

/**
 * Boundary between the UI and the SAS handshake.
 *
 * The current `libcore_ffi` surface exposes discovery and transfer sessions
 * only; it does not export the Rust pairing handshake. The Android app
 * therefore talks to this injectable gateway, and [LocalPreviewSasGateway]
 * supplies a local six-digit code so the sheet and its flow can be exercised.
 *
 * To ship real verification, provide an implementation that exchanges the
 * commitment/nonce with the peer through the Rust handshake and returns
 * `true` from [approve] only after both sides derived the same code.
 */
interface SasGateway {
    /** Whether this gateway is a local UI preview and must not create trust. */
    val isPreview: Boolean get() = false

    suspend fun challenge(peerIdHex: String): SasChallenge
    suspend fun approve(challenge: SasChallenge): Boolean
}

/**
 * Development stand-in. It deliberately does not claim to authenticate a
 * remote device; see [SasGateway].
 */
class LocalPreviewSasGateway : SasGateway {
    override val isPreview: Boolean = true

    private val random = SecureRandom()

    override suspend fun challenge(peerIdHex: String): SasChallenge =
        withContext(Dispatchers.Default) {
            delay(220)
            SasChallenge(
                peerIdHex = peerIdHex,
                code = String.format(Locale.US, "%06d", random.nextInt(1_000_000)),
                createdAtMillis = System.currentTimeMillis(),
            )
        }

    override suspend fun approve(challenge: SasChallenge): Boolean =
        withContext(Dispatchers.Default) {
            delay(260)
            System.currentTimeMillis() - challenge.createdAtMillis < CHALLENGE_LIFETIME_MS
        }

    private companion object {
        const val CHALLENGE_LIFETIME_MS = 120_000L
    }
}
