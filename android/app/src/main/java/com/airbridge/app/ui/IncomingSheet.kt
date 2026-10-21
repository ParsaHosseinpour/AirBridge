package com.airbridge.app.ui

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.airbridge.app.R
import com.airbridge.app.model.IncomingFilePhase
import com.airbridge.app.model.IncomingFileUi
import com.airbridge.app.model.IncomingPhase
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.app.model.formatBytes
import com.airbridge.app.model.formatCount
import com.airbridge.app.ui.theme.Amber
import com.airbridge.app.ui.theme.Danger
import com.airbridge.app.ui.theme.Mint
import com.airbridge.app.ui.theme.PrimaryCyan
import com.airbridge.app.ui.theme.SurfaceGlass
import com.airbridge.app.ui.theme.TextMuted
import com.airbridge.app.ui.theme.TextPrimary

/**
 * Bottom sheet for an inbound transfer.
 *
 * The Rust receiver starts moving bytes as soon as a peer connects, so the
 * first state is [IncomingPhase.WAITING] and the sheet makes the promise
 * explicit: nothing appears in Downloads until the user accepts *and* the
 * payload is verified.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun IncomingTransferSheet(
    incoming: IncomingTransferUi,
    onAccept: () -> Unit,
    onDecline: () -> Unit,
    onDismiss: () -> Unit,
) {
    val dismissible = incoming.phase.isTerminal

    ModalBottomSheet(
        onDismissRequest = { if (dismissible) onDismiss() },
        containerColor = SurfaceGlass,
        contentColor = TextPrimary,
        scrimColor = Color.Black.copy(alpha = 0.72f),
        dragHandle = { SheetHandle() },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .navigationBarsPadding()
                .padding(bottom = 20.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            IncomingProgressRing(
                incoming = incoming,
                modifier = Modifier.size(132.dp),
            )

            Spacer(Modifier.height(16.dp))
            Text(
                text = title(incoming),
                color = TextPrimary,
                fontSize = 20.sp,
                fontWeight = FontWeight.SemiBold,
                textAlign = TextAlign.Center,
            )
            Spacer(Modifier.height(6.dp))
            Text(
                text = subtitle(incoming),
                color = TextMuted,
                fontSize = 12.5.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(horizontal = 8.dp),
            )

            Spacer(Modifier.height(16.dp))
            if (incoming.phase == IncomingPhase.VERIFYING ||
                incoming.phase == IncomingPhase.VERIFIED ||
                incoming.pairingOnly
            ) {
                IncomingVerificationDetails(incoming)
            } else {
                IncomingDetails(incoming)
                if (incoming.files.isNotEmpty()) {
                    Spacer(Modifier.height(12.dp))
                    IncomingFileList(
                        rows = incoming.files,
                        phase = incoming.phase,
                    )
                }
            }

            if (incoming.error != null &&
                (incoming.phase == IncomingPhase.VERIFYING ||
                    incoming.phase == IncomingPhase.READY ||
                    incoming.phase == IncomingPhase.FAILED)
            ) {
                Spacer(Modifier.height(12.dp))
                IncomingErrorBanner(incoming.error)
            }

            Spacer(Modifier.height(18.dp))
            when (incoming.phase) {
                IncomingPhase.VERIFYING -> {
                    val code = incoming.sas
                    if (code != null) {
                        SasCode(code)
                        Spacer(Modifier.height(18.dp))
                    }
                    if (incoming.verificationSent) {
                        Row(
                            modifier = Modifier.fillMaxWidth(),
                            horizontalArrangement = Arrangement.Center,
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            CircularProgressIndicator(
                                modifier = Modifier.size(18.dp),
                                color = PrimaryCyan,
                                strokeWidth = 2.dp,
                            )
                            Spacer(Modifier.width(10.dp))
                            Text(
                                text = "Waiting for the other device…",
                                color = TextPrimary,
                                fontSize = 14.sp,
                                fontWeight = FontWeight.Medium,
                            )
                        }
                    } else {
                        PrimaryActionButton(
                            text = "Codes match",
                            iconRes = R.drawable.ic_check,
                            onClick = onAccept,
                            modifier = Modifier.fillMaxWidth(),
                            enabled = code != null,
                        )
                        Spacer(Modifier.height(8.dp))
                        SecondaryActionButton(
                            text = "Decline",
                            iconRes = R.drawable.ic_close,
                            onClick = onDecline,
                            modifier = Modifier.fillMaxWidth(),
                        )
                    }
                }

                IncomingPhase.WAITING -> {
                    PrimaryActionButton(
                        text = if (incoming.isBatch) {
                            "Accept ${formatCount(incoming.files.size)}"
                        } else {
                            "Accept file"
                        },
                        iconRes = R.drawable.ic_check,
                        onClick = onAccept,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Spacer(Modifier.height(8.dp))
                    SecondaryActionButton(
                        text = "Decline",
                        iconRes = R.drawable.ic_close,
                        onClick = onDecline,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }

                IncomingPhase.RECEIVING -> {
                    SecondaryActionButton(
                        text = "Cancel transfer",
                        iconRes = R.drawable.ic_close,
                        onClick = onDecline,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }

                IncomingPhase.READY -> {
                    PrimaryActionButton(
                        text = when {
                            incoming.error != null -> "Try saving again"
                            incoming.isBatch -> "Save ${formatCount(incoming.files.size)} to Downloads"
                            else -> "Save to Downloads"
                        },
                        iconRes = R.drawable.ic_check,
                        onClick = onAccept,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    Spacer(Modifier.height(8.dp))
                    SecondaryActionButton(
                        text = "Discard",
                        iconRes = R.drawable.ic_close,
                        onClick = onDecline,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }

                IncomingPhase.SAVING -> {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CircularProgressIndicator(
                            modifier = Modifier.size(18.dp),
                            color = PrimaryCyan,
                            strokeWidth = 2.dp,
                        )
                        Spacer(Modifier.width(10.dp))
                        Text(
                            text = if (incoming.isBatch) {
                                "Saving ${formatCount(incoming.files.size)} to Downloads…"
                            } else {
                                "Saving to Downloads…"
                            },
                            color = TextPrimary,
                            fontSize = 14.sp,
                            fontWeight = FontWeight.Medium,
                        )
                    }
                }

                IncomingPhase.SAVED, IncomingPhase.VERIFIED -> {
                    PrimaryActionButton(
                        text = "Done",
                        iconRes = R.drawable.ic_check,
                        onClick = onDismiss,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }

                IncomingPhase.FAILED, IncomingPhase.DECLINED -> {
                    SecondaryActionButton(
                        text = "Dismiss",
                        iconRes = R.drawable.ic_close,
                        onClick = onDismiss,
                        modifier = Modifier.fillMaxWidth(),
                    )
                }
            }
        }
    }
}

@Composable
private fun IncomingDetails(incoming: IncomingTransferUi) {
    GlassSurface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(18.dp),
    ) {
        Column(modifier = Modifier.padding(16.dp)) {
            if (incoming.isBatch) {
                IncomingInfoRow(
                    label = "Files",
                    value = formatCount(incoming.files.size),
                )
                Spacer(Modifier.height(10.dp))
            } else {
                IncomingInfoRow(
                    label = "File",
                    value = incoming.fileName,
                )
                Spacer(Modifier.height(10.dp))
            }
            IncomingInfoRow(
                label = "Size",
                value = when {
                    incoming.totalBytes != null -> formatBytes(incoming.totalBytes)
                    incoming.bytesTransferred > 0L ->
                        "${formatBytes(incoming.bytesTransferred)} so far"
                    else -> "Unknown until the transfer starts"
                },
            )
            if (incoming.isBatch) {
                Spacer(Modifier.height(10.dp))
                IncomingInfoRow(
                    label = "Progress",
                    value = buildString {
                        append("${incoming.completedFiles} of ${incoming.files.size} files")
                        incoming.etaText?.let { append(" · $it") }
                    },
                )
            }
            Spacer(Modifier.height(10.dp))
            IncomingInfoRow(
                label = "Destination",
                value = "Downloads/AirBridge",
            )
        }
    }
}

/**
 * Per-file rows for an inbound batch.
 *
 * The list has its own bounded scroll area: a folder offer can carry hundreds
 * of manifest entries, and nesting an unconstrained list in the sheet's
 * vertical scroll would compose every row at once.
 */
@Composable
private fun IncomingFileList(rows: List<IncomingFileUi>, phase: IncomingPhase) {
    LazyColumn(
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(max = 232.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        items(rows, key = { it.index }) { row ->
            IncomingFileRow(row = row, phase = phase)
        }
    }
}

@Composable
private fun IncomingFileRow(row: IncomingFileUi, phase: IncomingPhase) {
    val color = incomingFileColor(row.phase)
    val saving = phase == IncomingPhase.SAVING
    val fraction = when (row.phase) {
        IncomingFilePhase.PENDING -> 0f
        IncomingFilePhase.ACTIVE -> row.progress ?: 0f
        IncomingFilePhase.COMPLETED -> 1f
        IncomingFilePhase.FAILED, IncomingFilePhase.CANCELLED -> row.progress ?: 0f
    }
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(13.dp))
            .background(Color.White.copy(alpha = 0.04f))
            .padding(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(30.dp)
                .clip(RoundedCornerShape(9.dp))
                .background(color.copy(alpha = 0.12f)),
            contentAlignment = Alignment.Center,
        ) {
            Icon(
                painter = painterResource(incomingFileIcon(row.phase)),
                contentDescription = null,
                tint = color,
                modifier = Modifier.size(15.dp),
            )
        }
        Spacer(Modifier.width(9.dp))
        Column(modifier = Modifier.weight(1f)) {
            Text(
                text = row.name,
                color = TextPrimary,
                fontSize = 11.5.sp,
                fontWeight = FontWeight.Medium,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                text = when {
                    row.phase == IncomingFilePhase.FAILED && row.error != null -> row.error.orEmpty()
                    row.savedUri != null -> "Downloads/AirBridge" +
                        (row.folder?.let { "/$it" } ?: "")
                    row.folder != null -> row.folder.orEmpty()
                    else -> formatBytes(row.size)
                },
                color = if (row.phase == IncomingFilePhase.FAILED) Danger else TextMuted,
                fontSize = 9.5.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (row.phase == IncomingFilePhase.ACTIVE && !saving) {
                Spacer(Modifier.height(5.dp))
                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(3.dp)
                        .clip(CircleShape)
                        .background(Color.White.copy(alpha = 0.08f)),
                ) {
                    if (fraction > 0f) {
                        Box(
                            modifier = Modifier
                                .fillMaxWidth(fraction)
                                .height(3.dp)
                                .clip(CircleShape)
                                .background(color),
                        )
                    }
                }
            }
        }
        Spacer(Modifier.width(8.dp))
        Text(
            text = incomingFileStatus(row, phase),
            color = color,
            fontSize = 10.sp,
            maxLines = 1,
        )
    }
}

private fun incomingFileStatus(row: IncomingFileUi, phase: IncomingPhase): String = when {
    row.savedUri != null -> "Saved"
    row.phase == IncomingFilePhase.COMPLETED ->
        if (phase == IncomingPhase.SAVING) "Saving…" else "Received"
    row.phase == IncomingFilePhase.ACTIVE ->
        if (phase == IncomingPhase.SAVING) {
            "Saving…"
        } else {
            row.progress?.let { "${Math.round(it * 100f)}%" } ?: "Receiving…"
        }
    row.phase == IncomingFilePhase.PENDING ->
        if (phase == IncomingPhase.SAVING) "Waiting" else "Queued"
    row.phase == IncomingFilePhase.FAILED -> "Failed"
    row.phase == IncomingFilePhase.CANCELLED -> "Cancelled"
    else -> ""
}

private fun incomingFileColor(phase: IncomingFilePhase): Color = when (phase) {
    IncomingFilePhase.PENDING -> TextMuted
    IncomingFilePhase.ACTIVE -> PrimaryCyan
    IncomingFilePhase.COMPLETED -> Mint
    IncomingFilePhase.FAILED -> Danger
    IncomingFilePhase.CANCELLED -> TextMuted
}

private fun incomingFileIcon(phase: IncomingFilePhase): Int = when (phase) {
    IncomingFilePhase.COMPLETED -> R.drawable.ic_check
    IncomingFilePhase.FAILED -> R.drawable.ic_warning
    IncomingFilePhase.CANCELLED -> R.drawable.ic_close
    else -> R.drawable.ic_file
}

@Composable
private fun IncomingVerificationDetails(incoming: IncomingTransferUi) {
    GlassSurface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(18.dp),
    ) {
        Column(modifier = Modifier.padding(16.dp)) {
            IncomingInfoRow(
                label = "Device",
                value = incoming.peerIdHex
                    ?.take(16)
                    ?.uppercase()
                    ?: "Authenticating…",
            )
            Spacer(Modifier.height(10.dp))
            IncomingInfoRow(
                label = "Key",
                value = incoming.peerFingerprint
                    ?.take(16)
                    ?.uppercase()
                    ?: "SHA-256 pending",
            )
            Spacer(Modifier.height(10.dp))
            IncomingInfoRow(
                label = "Status",
                value = if (incoming.phase == IncomingPhase.VERIFIED) {
                    "Verified"
                } else {
                    "Compare the code on both screens"
                },
            )
        }
    }
}

@Composable
private fun IncomingInfoRow(label: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = label,
            color = TextMuted,
            fontSize = 12.sp,
            modifier = Modifier.width(86.dp),
        )
        Text(
            text = value,
            color = TextPrimary,
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
    }
}

@Composable
private fun IncomingErrorBanner(message: String) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .background(
                color = Danger.copy(alpha = 0.10f),
                shape = RoundedCornerShape(14.dp),
            )
            .border(1.dp, Danger.copy(alpha = 0.30f), RoundedCornerShape(14.dp))
            .padding(horizontal = 12.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Icon(
            painter = painterResource(R.drawable.ic_warning),
            contentDescription = null,
            tint = Danger,
            modifier = Modifier.size(15.dp),
        )
        Spacer(Modifier.width(9.dp))
        Text(
            text = message,
            color = Danger,
            fontSize = 11.5.sp,
        )
    }
}

/**
 * Circular receive progress.
 *
 * Uses a determinate gradient arc once the payload size is known (from the
 * progress event or the transport resume sidecar) and an indeterminate sweep
 * before that, so the card never invents a percentage.
 */
@Composable
private fun IncomingProgressRing(
    incoming: IncomingTransferUi,
    modifier: Modifier = Modifier,
) {
    val transition = rememberInfiniteTransition(label = "incoming-ring")
    val spin by transition.animateFloat(
        initialValue = 0f,
        targetValue = 360f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 1_400, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "incoming-spin",
    )

    val target = when (incoming.phase) {
        IncomingPhase.READY, IncomingPhase.SAVED, IncomingPhase.VERIFIED,
        IncomingPhase.FAILED, IncomingPhase.DECLINED -> 1f
        else -> incoming.progress ?: 0f
    }
    val animated by animateFloatAsState(
        targetValue = target,
        animationSpec = tween(durationMillis = 350),
        label = "incoming-progress",
    )

    val color = ringColor(incoming.phase)
    val indeterminate = incoming.progress == null &&
        (incoming.phase == IncomingPhase.VERIFYING ||
            incoming.phase == IncomingPhase.WAITING ||
            incoming.phase == IncomingPhase.RECEIVING ||
            incoming.phase == IncomingPhase.SAVING)

    Box(modifier = modifier, contentAlignment = Alignment.Center) {
        Canvas(modifier = Modifier.matchParentSize()) {
            val stroke = 4.5.dp.toPx()
            val inset = stroke / 2f
            val arcSize = Size(size.width - stroke, size.height - stroke)

            drawArc(
                color = Color.White.copy(alpha = 0.08f),
                startAngle = 0f,
                sweepAngle = 360f,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke),
            )

            // A soft halo behind the leading arc gives the ring depth on dark glass.
            drawArc(
                color = color.copy(alpha = 0.18f),
                startAngle = -90f,
                sweepAngle = 360f * (if (indeterminate) 0f else animated),
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke * 2.6f, cap = StrokeCap.Round),
            )

            if (indeterminate) {
                drawArc(
                    color = color,
                    startAngle = spin,
                    sweepAngle = 104f,
                    useCenter = false,
                    topLeft = Offset(inset, inset),
                    size = arcSize,
                    style = Stroke(width = stroke, cap = StrokeCap.Round),
                )
            } else {
                drawArc(
                    brush = Brush.sweepGradient(
                        colors = listOf(color, Mint, color),
                        center = Offset(size.width / 2f, size.height / 2f),
                    ),
                    startAngle = -90f,
                    sweepAngle = 360f * animated,
                    useCenter = false,
                    topLeft = Offset(inset, inset),
                    size = arcSize,
                    style = Stroke(width = stroke, cap = StrokeCap.Round),
                )
            }
        }

        Box(
            modifier = Modifier
                .size(94.dp)
                .background(color.copy(alpha = 0.10f), CircleShape),
            contentAlignment = Alignment.Center,
        ) {
            val percent = incoming.percent
            if (percent != null &&
                (incoming.phase == IncomingPhase.WAITING ||
                    incoming.phase == IncomingPhase.RECEIVING)
            ) {
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Text(
                        text = "$percent%",
                        color = TextPrimary,
                        fontSize = 23.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Text(
                        text = "received",
                        color = TextMuted,
                        fontSize = 10.sp,
                    )
                }
            } else {
                Icon(
                    painter = painterResource(phaseIcon(incoming.phase)),
                    contentDescription = null,
                    tint = color,
                    modifier = Modifier.size(30.dp),
                )
            }
        }
    }
}

private fun ringColor(phase: IncomingPhase): Color = when (phase) {
    IncomingPhase.READY, IncomingPhase.SAVED, IncomingPhase.VERIFIED -> Mint
    IncomingPhase.FAILED -> Danger
    IncomingPhase.DECLINED -> TextMuted
    IncomingPhase.SAVING -> Mint
    IncomingPhase.VERIFYING -> Amber
    else -> PrimaryCyan
}

private fun phaseIcon(phase: IncomingPhase): Int = when (phase) {
    IncomingPhase.READY, IncomingPhase.SAVED, IncomingPhase.VERIFIED -> R.drawable.ic_check
    IncomingPhase.FAILED -> R.drawable.ic_warning
    IncomingPhase.DECLINED -> R.drawable.ic_close
    IncomingPhase.VERIFYING -> R.drawable.ic_lock
    else -> R.drawable.ic_file
}

private fun title(incoming: IncomingTransferUi): String = when (incoming.phase) {
    IncomingPhase.VERIFYING -> "Verify this device"
    IncomingPhase.VERIFIED -> "Device verified"
    IncomingPhase.WAITING -> when {
        incoming.isBatch -> "${incoming.files.size} incoming files"
        else -> "Incoming file"
    }
    IncomingPhase.RECEIVING -> when {
        incoming.isBatch -> "Receiving ${incoming.files.size} files"
        else -> "Receiving…"
    }
    IncomingPhase.READY -> when {
        incoming.isBatch -> "${incoming.completedFiles} files ready to save"
        else -> "Ready to save"
    }
    IncomingPhase.SAVING -> if (incoming.isBatch) {
        "Saving ${incoming.files.size} files"
    } else {
        "Saving to Downloads"
    }
    IncomingPhase.SAVED -> if (incoming.files.size > 1) {
        "Saved ${incoming.completedFiles} files"
    } else {
        "Saved to Downloads"
    }
    IncomingPhase.FAILED -> "Transfer failed"
    IncomingPhase.DECLINED -> "Transfer declined"
}

private fun subtitle(incoming: IncomingTransferUi): String = when (incoming.phase) {
    IncomingPhase.VERIFYING ->
        "Compare this code with the other screen. Nothing is accepted until you confirm."
    IncomingPhase.VERIFIED ->
        "${incoming.peerIdHex ?: "The device"} is verified. No file was transferred."
    IncomingPhase.WAITING -> if (incoming.isBatch) {
        "A nearby device wants to send ${formatCount(incoming.files.size)}. " +
            "Nothing is saved until you accept."
    } else {
        "A nearby device is sending this file. Nothing is saved until you accept."
    }
    IncomingPhase.RECEIVING -> if (incoming.isBatch) {
        "Keep AirBridge open. ${formatCount(incoming.remainingFiles)} still to receive" +
            (incoming.etaText?.let { " · $it" } ?: "")
    } else {
        "Keep AirBridge open until the transfer finishes."
    }
    IncomingPhase.READY -> if (incoming.isBatch) {
        "The files arrived intact. Save them to your public Downloads folder."
    } else {
        "The file arrived intact. Save it to your public Downloads folder."
    }
    IncomingPhase.SAVING -> if (incoming.isBatch) {
        "Copying ${formatCount(incoming.files.size)} into Downloads/AirBridge."
    } else {
        "Copying the verified file into Downloads/AirBridge."
    }
    IncomingPhase.SAVED -> if (incoming.files.size > 1) {
        "${incoming.savedName ?: "${incoming.completedFiles} files"} are in Downloads/AirBridge."
    } else {
        "${incoming.savedName ?: incoming.fileName} is in Downloads/AirBridge."
    }
    IncomingPhase.FAILED ->
        incoming.error ?: "The transfer didn't finish. Nothing was saved."
    IncomingPhase.DECLINED ->
        "You declined this file. Nothing was written to Downloads."
}
