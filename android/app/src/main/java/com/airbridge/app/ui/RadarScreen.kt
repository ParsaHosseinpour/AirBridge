package com.airbridge.app.ui

import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyRow
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.airbridge.app.R
import com.airbridge.app.model.DiscoveryUiState
import com.airbridge.app.model.NearbyPeer
import com.airbridge.app.model.SelectedFile
import com.airbridge.app.model.TransferUi
import com.airbridge.app.model.formatBytes
import com.airbridge.app.model.formatCount
import com.airbridge.app.model.initials
import com.airbridge.core.DiscoveryTier
import com.airbridge.core.TierState
import com.airbridge.app.ui.theme.Amber
import com.airbridge.app.ui.theme.Danger
import com.airbridge.app.ui.theme.Mint
import com.airbridge.app.ui.theme.PrimaryCyan
import com.airbridge.app.ui.theme.TextMuted
import com.airbridge.app.ui.theme.TextPrimary

@Composable
fun RadarScreen(
    displayName: String,
    peers: List<NearbyPeer>,
    discovery: DiscoveryUiState,
    transfer: TransferUi?,
    selectedFiles: List<SelectedFile>,
    onPickFiles: () -> Unit,
    onPeerClick: (NearbyPeer) -> Unit,
    onRemoveFile: (SelectedFile) -> Unit,
    onClearFiles: () -> Unit,
    onCancelTransfer: () -> Unit,
    onDismissTransfer: () -> Unit,
    onProfile: () -> Unit,
    onRefresh: () -> Unit,
    onDropFiles: (List<Uri>) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(modifier = modifier.fillMaxSize()) {
        Header(
            displayName = displayName,
            discovery = discovery,
            onProfile = onProfile,
            onRefresh = onRefresh,
        )

        discovery.error?.let { message ->
            ErrorBanner(message = message, onRetry = onRefresh)
        }

        RadarStage(
            peers = peers,
            discovery = discovery,
            transfer = transfer,
            displayName = displayName,
            onPeerClick = onPeerClick,
            onCancelTransfer = onCancelTransfer,
            onDismissTransfer = onDismissTransfer,
            onDropFiles = onDropFiles,
            modifier = Modifier
                .fillMaxWidth()
                .weight(1f),
        )

        TierStatusRow(discovery = discovery)

        Spacer(Modifier.height(10.dp))

        if (selectedFiles.isEmpty()) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 20.dp),
            ) {
                PrimaryActionButton(
                    text = "Choose files to send",
                    iconRes = R.drawable.ic_add,
                    onClick = onPickFiles,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(10.dp))
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.Center,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Icon(
                        painter = painterResource(R.drawable.ic_drag),
                        contentDescription = null,
                        tint = TextMuted,
                        modifier = Modifier.size(14.dp),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = "or drag files from another app onto the radar",
                        color = TextMuted,
                        fontSize = 11.sp,
                    )
                }
            }
        } else {
            FileShelf(
                files = selectedFiles,
                onRemoveFile = onRemoveFile,
                onClearFiles = onClearFiles,
            )
            Spacer(Modifier.height(6.dp))
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 20.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    text = "Tap a nearby device to send",
                    color = TextMuted,
                    fontSize = 11.5.sp,
                )
                Spacer(Modifier.weight(1f))
                TextButton(onClick = onPickFiles) {
                    Icon(
                        painter = painterResource(R.drawable.ic_add),
                        contentDescription = null,
                        tint = PrimaryCyan,
                        modifier = Modifier.size(14.dp),
                    )
                    Spacer(Modifier.width(4.dp))
                    Text(
                        text = "Add files",
                        color = PrimaryCyan,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            }
        }

        Spacer(Modifier.height(8.dp))
    }
}

@Composable
private fun Header(
    displayName: String,
    discovery: DiscoveryUiState,
    onProfile: () -> Unit,
    onRefresh: () -> Unit,
) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 20.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(44.dp)
                .clip(CircleShape)
                .background(Brush.linearGradient(listOf(Color(0xFF1E7FB8), Color(0xFF0B2A45))))
                .border(1.dp, Color.White.copy(alpha = 0.16f), CircleShape)
                .clickable(onClick = onProfile),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = displayName.initials(),
                color = Color.White,
                fontSize = 15.sp,
                fontWeight = FontWeight.SemiBold,
            )
        }

        Spacer(Modifier.width(12.dp))

        Column(modifier = Modifier.weight(1f)) {
            Text(
                text = "AirBridge",
                color = TextPrimary,
                fontSize = 19.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Spacer(Modifier.height(3.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                val dotColor = when {
                    discovery.error != null -> Danger
                    discovery.running -> Mint
                    discovery.starting -> PrimaryCyan
                    else -> TextMuted
                }
                PulseDot(color = dotColor)
                Spacer(Modifier.width(6.dp))
                Text(
                    text = headerStatus(discovery),
                    color = TextMuted,
                    fontSize = 11.5.sp,
                )
            }
        }

        IconButton(onClick = onRefresh) {
            Icon(
                painter = painterResource(R.drawable.ic_refresh),
                contentDescription = "Refresh discovery",
                tint = TextMuted,
                modifier = Modifier.size(20.dp),
            )
        }
    }
}

private fun headerStatus(discovery: DiscoveryUiState): String = when {
    discovery.error != null -> "Setup needed"
    discovery.starting -> "Starting discovery…"
    discovery.running -> "Visible to nearby devices"
    else -> "Sharing is paused"
}

@Composable
private fun ErrorBanner(message: String, onRetry: () -> Unit) {
    GlassSurface(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 20.dp, vertical = 2.dp),
        shape = RoundedCornerShape(18.dp),
        alpha = 0.07f,
        borderAlpha = 0.12f,
    ) {
        Row(
            modifier = Modifier.padding(start = 14.dp, top = 12.dp, bottom = 12.dp, end = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_warning),
                contentDescription = null,
                tint = Amber,
                modifier = Modifier.size(18.dp),
            )
            Spacer(Modifier.width(10.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = "Discovery needs attention",
                    color = TextPrimary,
                    fontSize = 13.sp,
                    fontWeight = FontWeight.SemiBold,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    text = message,
                    color = TextMuted,
                    fontSize = 11.sp,
                    maxLines = 3,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            TextButton(onClick = onRetry) {
                Text(
                    text = "Retry",
                    color = PrimaryCyan,
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Medium,
                )
            }
        }
    }
}

@Composable
private fun TierStatusRow(discovery: DiscoveryUiState) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 20.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        StatusPill(
            label = "Wi-Fi",
            dotColor = tierColor(discovery.stateOf(DiscoveryTier.MDNS)),
        )
        StatusPill(
            label = "Bluetooth",
            dotColor = tierColor(discovery.stateOf(DiscoveryTier.BLE)),
        )
        StatusPill(
            label = "Scan",
            dotColor = tierColor(discovery.stateOf(DiscoveryTier.SUBNET)),
        )
    }
}

private fun tierColor(state: TierState): Color = when (state) {
    TierState.Active -> Mint
    TierState.Starting -> PrimaryCyan
    is TierState.Degraded -> Amber
    is TierState.Failed -> Danger
    TierState.Stopped -> TextMuted.copy(alpha = 0.45f)
}

@Composable
private fun FileShelf(
    files: List<SelectedFile>,
    onRemoveFile: (SelectedFile) -> Unit,
    onClearFiles: () -> Unit,
) {
    Column(modifier = Modifier.fillMaxWidth()) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(start = 20.dp, end = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                text = "Ready to send",
                color = TextPrimary,
                fontSize = 13.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Spacer(Modifier.width(8.dp))
            Text(
                text = formatCount(files.size),
                color = TextMuted,
                fontSize = 11.sp,
            )
            Spacer(Modifier.weight(1f))
            TextButton(onClick = onClearFiles) {
                Text(
                    text = "Clear",
                    color = TextMuted,
                    fontSize = 12.sp,
                )
            }
        }
        LazyRow(
            modifier = Modifier.fillMaxWidth(),
            contentPadding = PaddingValues(horizontal = 20.dp),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            items(files, key = { it.key }) { file ->
                FileCard(file = file, onRemove = { onRemoveFile(file) })
            }
        }
    }
}

@Composable
private fun FileCard(file: SelectedFile, onRemove: () -> Unit) {
    GlassSurface(
        modifier = Modifier.width(188.dp),
        shape = RoundedCornerShape(18.dp),
    ) {
        Row(
            modifier = Modifier.padding(start = 10.dp, top = 10.dp, bottom = 10.dp, end = 2.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(42.dp)
                    .clip(RoundedCornerShape(13.dp))
                    .background(PrimaryCyan.copy(alpha = 0.12f)),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_file),
                    contentDescription = null,
                    tint = PrimaryCyan,
                    modifier = Modifier.size(20.dp),
                )
            }
            Spacer(Modifier.width(10.dp))
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = file.name,
                    color = TextPrimary,
                    fontSize = 12.sp,
                    fontWeight = FontWeight.Medium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Spacer(Modifier.height(2.dp))
                Text(
                    text = formatBytes(file.size),
                    color = TextMuted,
                    fontSize = 10.sp,
                )
            }
            IconButton(
                onClick = onRemove,
                modifier = Modifier.size(30.dp),
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_close),
                    contentDescription = "Remove ${file.name}",
                    tint = TextMuted,
                    modifier = Modifier.size(13.dp),
                )
            }
        }
    }
}
