// Hand-written adapters over the generated UniFFI surface. This file is not
// generated: it belongs to the repository, and the Android Gradle module adds
// it (plus the generated bindings) as a source directory.
package com.airbridge.core

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow

/**
 * Streams [DiscoveryEvent]s from a running [PeerDiscoveryEngine] as a cold
 * [Flow].
 *
 * The engine exposes a pull-based `nextEvent()`; this adapter turns it into
 * the coroutine-native shape. Collection suspends until the next event is
 * available, so the collector's dispatcher is never blocked, and events are
 * requested one at a time — a slow collector applies backpressure instead of
 * building an unbounded queue. The coordinator's internal ring does drop
 * events when a consumer falls behind; that loss is reported explicitly as
 * [DiscoveryEvent.Lagged], and [PeerDiscoveryEngine.peers] remains the
 * authoritative snapshot.
 *
 * The flow completes after [DiscoveryEvent.Shutdown], when the stream is
 * closed. A failure of the underlying call is thrown as [FfiException] and
 * terminates the flow.
 *
 * Collect one flow per engine. The stream is single-consumer: collecting two
 * flows over the same engine makes them steal events from each other. Fan out
 * with `shareIn`/`SharedFlow` inside the app instead.
 *
 * ```kotlin
 * lifecycleScope.launch {
 *     engine.events().collect { event ->
 *         when (event) {
 *             is DiscoveryEvent.PeerFound -> render(event.peer)
 *             is DiscoveryEvent.Lagged -> render(engine.peers())
 *             else -> Unit
 *         }
 *     }
 * }
 * ```
 */
public fun PeerDiscoveryEngine.events(): Flow<DiscoveryEvent> = flow {
    while (true) {
        val event = nextEvent() ?: break
        emit(event)
        if (event is DiscoveryEvent.Shutdown) break
    }
}

/**
 * Streams [TransferEvent]s from a [FileTransferSession] as a cold [Flow].
 *
 * This is the progress channel: the session is driven by [FileTransferSession.run],
 * which resolves when the transfer reaches a terminal state, while this flow
 * carries the throttled [TransferEvent.Progress] samples and the terminal
 * event itself. Progress numbers count bytes seen by the stream adapter
 * (framing included), so treat them as movement, not as an exact percentage.
 *
 * The flow completes after the terminal event ([TransferEvent.Completed],
 * [TransferEvent.Failed] or [TransferEvent.Cancelled]); the session closes
 * its stream at the same moment. Collect one flow per session.
 *
 * ```kotlin
 * val progress = launch {
 *     session.events().collect { event ->
 *         if (event is TransferEvent.Progress) showProgress(event)
 *     }
 * }
 * try {
 *     val report = session.run()
 *     showDone(report)
 * } finally {
 *     progress.join()
 * }
 * ```
 */
public fun FileTransferSession.events(): Flow<TransferEvent> = flow {
    while (true) {
        val event = nextEvent() ?: break
        emit(event)
        when (event) {
            is TransferEvent.Completed,
            is TransferEvent.Failed,
            TransferEvent.Cancelled -> break
            else -> Unit
        }
    }
}

/**
 * Streams [BatchEvent]s from a [BatchTransferSession] as a cold [Flow].
 *
 * The session is driven by [BatchTransferSession.run], which resolves with the
 * final [BatchReport], while this flow carries the throttled per-file progress
 * samples and the terminal event. The flow completes after [BatchEvent.Finished],
 * [BatchEvent.Failed] or [BatchEvent.Cancelled]; collect one flow per session.
 *
 * [BatchEvent.Lagged] means the event ring dropped samples. Re-read
 * [BatchTransferSession.snapshot] for the authoritative queue state rather
 * than reconstructing it from the stream.
 */
public fun BatchTransferSession.events(): Flow<BatchEvent> = flow {
    while (true) {
        val event = nextEvent() ?: break
        emit(event)
        when (event) {
            is BatchEvent.Finished,
            is BatchEvent.Failed,
            BatchEvent.Cancelled -> break
            else -> Unit
        }
    }
}
