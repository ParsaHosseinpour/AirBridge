package com.airbridge.app.core

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * The resume sidecar is the only local hint for the inbound payload size, so
 * its offsets are pinned here against `libcore_transport::state`.
 */
class InboundParsingTest {
    @get:Rule
    val folder = TemporaryFolder()

    @Test
    fun readsTotalLengthFromResumeSidecar() {
        val destination = File(folder.root, "incoming.bin")
        val state = stateBytes(total = 12_345L, payloadId = 0xDEAD_BEEFL)
        File(destination.absolutePath + ".part.state").writeBytes(state)

        assertEquals(12_345L, InboundResumeHints.totalBytes(destination))
    }

    @Test
    fun ignoresMissingTruncatedAndForeignSidecars() {
        val destination = File(folder.root, "incoming.bin")
        assertNull(InboundResumeHints.totalBytes(destination))

        val statePath = File(destination.absolutePath + ".part.state")
        statePath.writeBytes(stateBytes(total = 99L).copyOf(40))
        assertNull(InboundResumeHints.totalBytes(destination))

        val foreign = stateBytes(total = 99L)
        foreign[0] = 'X'.code.toByte()
        statePath.writeBytes(foreign)
        assertNull(InboundResumeHints.totalBytes(destination))
    }

    @Test
    fun ignoresZeroLengthPayloadWhenSizingTheRing() {
        val destination = File(folder.root, "empty.bin")
        File(destination.absolutePath + ".part.state")
            .writeBytes(stateBytes(total = 0L))

        assertNull(InboundResumeHints.totalBytes(destination))
    }

    @Test
    fun sniffsCommonPayloadSignatures() {
        assertEquals(".png", sniff("image", byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)))
        assertEquals(".pdf", sniff("document", "%PDF-1.7\n".toByteArray()))
        assertEquals(".zip", sniff("archive", byteArrayOf(0x50, 0x4B, 0x03, 0x04, 0x14, 0x00)))
        assertEquals(".txt", sniff("notes", "hello from a nearby device\n".toByteArray()))
        assertEquals(".bin", sniff("opaque", byteArrayOf(0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07)))
    }

    private fun sniff(name: String, header: ByteArray): String {
        val file = File(folder.root, "$name.bin")
        file.writeBytes(header)
        return IncomingFileKinds.sniffExtension(file)
    }

    private fun stateBytes(total: Long, payloadId: Long = 7L): ByteArray {
        val state = ByteArray(98)
        byteArrayOf('A'.code.toByte(), 'B'.code.toByte(), 'R'.code.toByte(), 'S'.code.toByte())
            .copyInto(state, 0)
        state[4] = 1
        val buffer = ByteBuffer.wrap(state).order(ByteOrder.BIG_ENDIAN)
        buffer.putLong(6, payloadId)
        buffer.putLong(14, total)
        return state
    }
}
