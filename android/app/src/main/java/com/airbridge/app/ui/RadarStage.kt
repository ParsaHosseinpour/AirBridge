package com.airbridge.app.ui

import android.net.Uri
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.draganddrop.dragAndDropTarget
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.scale
import androidx.compose.ui.draganddrop.DragAndDropEvent
import androidx.compose.ui.draganddrop.DragAndDropTarget
import androidx.compose.ui.draganddrop.toAndroidDragEvent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathEffect
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.airbridge.app.R
import com.airbridge.app.model.DiscoveryUiState
import com.airbridge.app.model.NearbyPeer
import com.airbridge.app.model.TransferPhase
import com.airbridge.app.model.TransferUi
import com.airbridge.app.model.initials
import com.airbridge.app.ui.theme.Danger
import com.airbridge.app.ui.theme.Mint
import com.airbridge.app.ui.theme.PrimaryCyan
import com.airbridge.app.ui.theme.TextMuted
import com.airbridge.app.ui.theme.TextPrimary
import kotlin.math.PI
import kotlin.math.cos
import kotlin.math.min
import kotlin.math.roundToInt
import kotlin.math.sin

/**
 * The whole radar surface: rings, central node, peer bubbles and the
 * drag-and-drop overlay.
 */
@Composable
fun RadarStage(
    peers: List<NearbyPeer>,
    discovery: DiscoveryUiState,
    transfer: TransferUi?,
    displayName: String,
    onPeerClick: (NearbyPeer) -> Unit,
    onCancelTransfer: () -> Unit,
    onDismissTransfer: () -> Unit,
    onDropFiles: (List<Uri>) -> Unit,
    modifier: Modifier = Modifier,
) {
    var dragActive by remember { mutableStateOf(false) }

    val dropTarget = remember(onDropFiles) {
        object : DragAndDropTarget {
            override fun onDrop(event: DragAndDropEvent): Boolean {
                val clip = event.toAndroidDragEvent().clipData ?: return false
                val uris = buildList {
                    for (index in 0 until clip.itemCount) {
                        clip.getItemAt(index).uri?.let(::add)
                    }
                }
                if (uris.isEmpty()) return false
                onDropFiles(uris)
                return true
            }

            override fun onEntered(event: DragAndDropEvent) {
                dragActive = true
            }

            override fun onExited(event: DragAndDropEvent) {
                dragActive = false
            }

            override fun onEnded(event: DragAndDropEvent) {
                dragActive = false
            }
        }
    }

    BoxWithConstraints(
        modifier = modifier
            .fillMaxSize()
            .dragAndDropTarget(
                shouldStartDragAndDrop = { event ->
                    event.toAndroidDragEvent().clipData != null
                },
                target = dropTarget,
            ),
    ) {
        val density = LocalDensity.current
        val stageWidth = with(density) { maxWidth.toPx() }
        val stageHeight = with(density) { maxHeight.toPx() }
        val center = Offset(stageWidth / 2f, stageHeight / 2f)
        val orbitRadius = (min(stageWidth, stageHeight) / 2f) - with(density) { 62.dp.toPx() }

        RadarSweep(
            active = discovery.running,
            modifier = Modifier.fillMaxSize(),
        )

        if (dragActive) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(10.dp)
                    .clip(RoundedCornerShape(30.dp))
                    .background(PrimaryCyan.copy(alpha = 0.08f))
                    .border(
                        1.5.dp,
                        PrimaryCyan.copy(alpha = 0.55f),
                        RoundedCornerShape(30.dp),
                    ),
            ) {
                Text(
                    text = "Drop files to add them",
                    color = PrimaryCyan,
                    fontSize = 14.sp,
                    fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.align(Alignment.Center),
                )
            }
        }

        if (peers.isEmpty() && !dragActive) {
            Column(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 4.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(
                    text = if (discovery.starting) "Waking up the radar…" else "No one nearby yet",
                    color = TextPrimary.copy(alpha = 0.85f),
                    fontSize = 13.sp,
                    fontWeight = FontWeight.Medium,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    text = "Keep both devices unlocked and on the same Wi-Fi",
                    color = TextMuted,
                    fontSize = 11.sp,
                    textAlign = TextAlign.Center,
                )
            }
        }

        val visible = peers.take(8)
        visible.forEachIndexed { index, peer ->
            PeerBubble(
                peer = peer,
                index = index,
                count = visible.size,
                center = center,
                orbitRadius = orbitRadius,
                onClick = { onPeerClick(peer) },
            )
        }

        CentralNode(
            displayName = displayName,
            transfer = transfer,
            onCancelTransfer = onCancelTransfer,
            onDismissTransfer = onDismissTransfer,
            modifier = Modifier.align(Alignment.Center),
        )
    }
}

@Composable
private fun RadarSweep(active: Boolean, modifier: Modifier = Modifier) {
    val transition = rememberInfiniteTransition(label = "radar-sweep")
    val sweep by transition.animateFloat(
        initialValue = 0f,
        targetValue = 360f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 9_000, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "sweep-angle",
    )

    Canvas(modifier) {
        val center = Offset(size.width / 2f, size.height / 2f)
        val orbit = (min(size.width, size.height) / 2f) - 62.dp.toPx()
        if (orbit <= 0f) return@Canvas

        drawCircle(
            brush = Brush.radialGradient(
                colors = listOf(PrimaryCyan.copy(alpha = 0.10f), Color.Transparent),
                center = center,
                radius = orbit * 1.1f,
            ),
            radius = orbit * 1.1f,
            center = center,
        )

        val rings = listOf(0.52f, 0.72f, 0.90f, 1.0f)
        rings.forEachIndexed { index, fraction ->
            val radius = orbit * fraction
            val dashed = index == 1
            drawCircle(
                color = Color.White.copy(alpha = if (index == rings.lastIndex) 0.14f else 0.07f),
                radius = radius,
                center = center,
                style = Stroke(
                    width = if (index == rings.lastIndex) 1.4.dp.toPx() else 1.dp.toPx(),
                    pathEffect = if (dashed) {
                        PathEffect.dashPathEffect(
                            floatArrayOf(3.dp.toPx(), 11.dp.toPx()),
                            0f,
                        )
                    } else {
                        null
                    },
                ),
            )
        }

        repeat(12) { tick ->
            val angle = Math.toRadians((tick * 30).toDouble())
            val outer = orbit
            val inner = orbit - 5.dp.toPx()
            drawLine(
                color = Color.White.copy(alpha = 0.12f),
                start = Offset(
                    center.x + (cos(angle) * inner).toFloat(),
                    center.y + (sin(angle) * inner).toFloat(),
                ),
                end = Offset(
                    center.x + (cos(angle) * outer).toFloat(),
                    center.y + (sin(angle) * outer).toFloat(),
                ),
                strokeWidth = 1.dp.toPx(),
            )
        }

        if (active) {
            for (trail in 0 until 18) {
                val angle = sweep - trail * 3.4f
                drawArc(
                    color = PrimaryCyan.copy(alpha = 0.30f * (1f - trail / 18f)),
                    startAngle = angle,
                    sweepAngle = 2.6f,
                    useCenter = false,
                    topLeft = Offset(center.x - orbit, center.y - orbit),
                    size = Size(orbit * 2f, orbit * 2f),
                    style = Stroke(width = 2.dp.toPx(), cap = StrokeCap.Round),
                )
            }
            val lead = Math.toRadians(sweep.toDouble())
            drawCircle(
                color = PrimaryCyan.copy(alpha = 0.9f),
                radius = 3.dp.toPx(),
                center = Offset(
                    center.x + (cos(lead) * orbit).toFloat(),
                    center.y + (sin(lead) * orbit).toFloat(),
                ),
            )
        }

        val blips = listOf(
            Triple(0.52f, 26f, 0.35f),
            Triple(0.72f, 158f, 0.28f),
            Triple(0.90f, 250f, 0.30f),
            Triple(1.0f, 310f, 0.22f),
        )
        blips.forEach { (fraction, degrees, alpha) ->
            val angle = Math.toRadians(degrees.toDouble())
            val radius = orbit * fraction
            drawCircle(
                color = Color.White.copy(alpha = alpha),
                radius = 1.6.dp.toPx(),
                center = Offset(
                    center.x + (cos(angle) * radius).toFloat(),
                    center.y + (sin(angle) * radius).toFloat(),
                ),
            )
        }
    }
}

@Composable
private fun CentralNode(
    displayName: String,
    transfer: TransferUi?,
    onCancelTransfer: () -> Unit,
    onDismissTransfer: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier = modifier,
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Box(
            modifier = Modifier.size(150.dp),
            contentAlignment = Alignment.Center,
        ) {
            Box(
                modifier = Modifier
                    .size(140.dp)
                    .background(
                        brush = Brush.radialGradient(
                            colors = listOf(PrimaryCyan.copy(alpha = 0.20f), Color.Transparent),
                        ),
                        shape = CircleShape,
                    ),
            )
            if (transfer != null) {
                TransferRing(
                    transfer = transfer,
                    modifier = Modifier.size(132.dp),
                )
            }
            Box(
                modifier = Modifier
                    .size(102.dp)
                    .clip(CircleShape)
                    .background(
                        Brush.linearGradient(
                            colors = listOf(Color(0xFF1E7FB8), Color(0xFF0B2A45)),
                            start = Offset(0f, 0f),
                            end = Offset(102f, 102f),
                        ),
                    )
                    .border(2.dp, Color.White.copy(alpha = 0.20f), CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = displayName.initials(),
                    color = Color.White,
                    fontSize = 32.sp,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            Box(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .offset(y = (-8).dp)
                    .size(20.dp)
                    .background(PrimaryCyan.copy(alpha = 0.25f), CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Box(
                    modifier = Modifier
                        .size(10.dp)
                        .background(Mint, CircleShape),
                )
            }
        }

        Spacer(Modifier.height(2.dp))
        Text(
            text = if (transfer == null) displayName else transfer.peerName,
            color = TextPrimary,
            fontSize = 15.sp,
            fontWeight = FontWeight.SemiBold,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.width(220.dp),
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(2.dp))

        if (transfer == null) {
            Text(
                text = "This device",
                color = TextMuted,
                fontSize = 11.sp,
            )
        } else {
            Text(
                text = transferStatus(transfer),
                color = transferColor(transfer),
                fontSize = 12.sp,
                fontWeight = FontWeight.Medium,
            )
            Spacer(Modifier.height(2.dp))
            Text(
                text = transfer.fileName,
                color = TextMuted,
                fontSize = 10.sp,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.width(210.dp),
                textAlign = TextAlign.Center,
            )
            Spacer(Modifier.height(2.dp))
            if (transfer.phase.isActive) {
                TextButton(
                    onClick = onCancelTransfer,
                    enabled = transfer.phase != TransferPhase.CANCELLING,
                    modifier = Modifier.height(30.dp),
                ) {
                    Text(
                        text = if (transfer.phase == TransferPhase.CANCELLING) "Stopping…" else "Cancel",
                        color = Danger,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            } else {
                TextButton(
                    onClick = onDismissTransfer,
                    modifier = Modifier.height(30.dp),
                ) {
                    Text(
                        text = "Dismiss",
                        color = PrimaryCyan,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            }
        }
    }
}

@Composable
private fun TransferRing(transfer: TransferUi, modifier: Modifier = Modifier) {
    val transition = rememberInfiniteTransition(label = "transfer-ring")
    val spin by transition.animateFloat(
        initialValue = 0f,
        targetValue = 360f,
        animationSpec = infiniteRepeatable(
            animation = tween(durationMillis = 1_300, easing = LinearEasing),
            repeatMode = RepeatMode.Restart,
        ),
        label = "spin",
    )
    val color = transferColor(transfer)
    val progress = transfer.progress

    Canvas(modifier) {
        val stroke = 3.5.dp.toPx()
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

        if (progress != null) {
            drawArc(
                color = color.copy(alpha = 0.25f),
                startAngle = -90f,
                sweepAngle = 360f * progress,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke * 2.4f, cap = StrokeCap.Round),
            )
            drawArc(
                brush = Brush.sweepGradient(
                    colors = listOf(color, Mint, color),
                    center = Offset(size.width / 2f, size.height / 2f),
                ),
                startAngle = -90f,
                sweepAngle = 360f * progress,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke, cap = StrokeCap.Round),
            )
        } else {
            drawArc(
                color = color,
                startAngle = spin,
                sweepAngle = 96f,
                useCenter = false,
                topLeft = Offset(inset, inset),
                size = arcSize,
                style = Stroke(width = stroke, cap = StrokeCap.Round),
            )
        }
    }
}

private fun transferColor(transfer: TransferUi): Color = when (transfer.phase) {
    TransferPhase.FAILED -> Danger
    TransferPhase.COMPLETED -> Mint
    TransferPhase.CANCELLED -> TextMuted
    else -> PrimaryCyan
}

private fun transferStatus(transfer: TransferUi): String = when (transfer.phase) {
    TransferPhase.CONNECTING -> "Connecting…"
    TransferPhase.RUNNING -> {
        val percent = transfer.percent
        if (percent != null) "${transfer.verb} $percent%" else "${transfer.verb}…"
    }
    TransferPhase.CANCELLING -> "Stopping…"
    TransferPhase.COMPLETED -> transfer.completedVerb
    TransferPhase.FAILED -> "Couldn't send"
    TransferPhase.CANCELLED -> "Cancelled"
}

@Composable
private fun PeerBubble(
    peer: NearbyPeer,
    index: Int,
    count: Int,
    center: Offset,
    orbitRadius: Float,
    onClick: () -> Unit,
) {
    val density = LocalDensity.current
    val bubbleWidth = 96.dp
    val bubbleWidthPx = with(density) { bubbleWidth.toPx() }
    val bubbleHeightPx = with(density) { 110.dp.toPx() }

    val step = 360f / count.coerceAtLeast(1)
    val jitter = (peer.idHex.hashCode().mod(13)) - 6f
    val angle = -90f + step * index + jitter
    val radians = angle.toDouble() * PI / 180.0
    val x = center.x + (cos(radians) * orbitRadius).toFloat() - bubbleWidthPx / 2f
    val y = center.y + (sin(radians) * orbitRadius).toFloat() - bubbleHeightPx / 2f

    var appeared by remember(peer.idHex) { mutableStateOf(false) }
    LaunchedEffect(peer.idHex) { appeared = true }
    val enter by animateFloatAsState(
        targetValue = if (appeared) 1f else 0f,
        animationSpec = spring(dampingRatio = 0.55f, stiffness = 220f),
        label = "peer-enter",
    )

    val floatTransition = rememberInfiniteTransition(label = "peer-float-${peer.idHex}")
    val drift by floatTransition.animateFloat(
        initialValue = -3f,
        targetValue = 3f,
        animationSpec = infiniteRepeatable(
            animation = tween(
                durationMillis = 2_200 + (index % 3) * 420,
                easing = FastOutSlowInEasing,
            ),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "peer-drift",
    )

    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val pressScale by animateFloatAsState(
        targetValue = if (pressed) 0.93f else 1f,
        label = "peer-press",
    )

    Column(
        modifier = Modifier
            .offset {
                IntOffset(
                    x.roundToInt(),
                    (y + drift * density.density).roundToInt(),
                )
            }
            .width(bubbleWidth)
            .scale(enter * pressScale)
            .alpha(enter)
            .clickable(
                interactionSource = interaction,
                indication = null,
                onClick = onClick,
            ),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Top,
    ) {
        Box(
            modifier = Modifier.size(64.dp),
            contentAlignment = Alignment.Center,
        ) {
            val palette = avatarPalette(peer.idHex)
            Box(
                modifier = Modifier
                    .size(60.dp)
                    .background(
                        brush = Brush.radialGradient(
                            colors = listOf(palette.first().copy(alpha = 0.40f), Color.Transparent),
                        ),
                        shape = CircleShape,
                    ),
            )
            Box(
                modifier = Modifier
                    .size(54.dp)
                    .clip(CircleShape)
                    .background(Brush.linearGradient(palette))
                    .border(
                        1.dp,
                        if (peer.isTrusted) {
                            Mint.copy(alpha = 0.9f)
                        } else {
                            Color.White.copy(alpha = 0.25f)
                        },
                        CircleShape,
                    ),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    text = peer.initials,
                    color = Color.White,
                    fontSize = 18.sp,
                    fontWeight = FontWeight.SemiBold,
                )
            }
            if (peer.isTrusted) {
                Box(
                    modifier = Modifier
                        .align(Alignment.BottomEnd)
                        .size(20.dp)
                        .background(Color(0xFF0B2A22), CircleShape)
                        .border(1.dp, Mint, CircleShape),
                    contentAlignment = Alignment.Center,
                ) {
                    Icon(
                        painter = painterResource(R.drawable.ic_check),
                        contentDescription = null,
                        modifier = Modifier.size(12.dp),
                        tint = Mint,
                    )
                }
            }
        }
        Spacer(Modifier.height(2.dp))
        Text(
            text = peer.name,
            color = TextPrimary.copy(alpha = 0.92f),
            fontSize = 11.sp,
            fontWeight = FontWeight.Medium,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            textAlign = TextAlign.Center,
            modifier = Modifier.width(bubbleWidth),
        )
    }
}

internal val AvatarPalettes = listOf(
    listOf(Color(0xFF5AC8FA), Color(0xFF2F7DC4)),
    listOf(Color(0xFF7DE2C3), Color(0xFF2F9E8F)),
    listOf(Color(0xFFFFB86B), Color(0xFFE07A3F)),
    listOf(Color(0xFFC7A6FF), Color(0xFF7C5CD6)),
    listOf(Color(0xFFFF9BB3), Color(0xFFC75478)),
    listOf(Color(0xFF9BE15D), Color(0xFF4E9F3D)),
)

internal fun avatarPalette(idHex: String): List<Color> =
    AvatarPalettes[(idHex.hashCode().mod(AvatarPalettes.size))]
