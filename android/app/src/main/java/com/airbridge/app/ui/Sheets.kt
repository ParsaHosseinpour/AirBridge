package com.airbridge.app.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.airbridge.app.R
import com.airbridge.app.model.NearbyPeer
import com.airbridge.app.model.SelectedFile
import com.airbridge.app.model.formatCount
import com.airbridge.app.model.initials
import com.airbridge.app.ui.theme.Amber
import com.airbridge.app.ui.theme.Danger
import com.airbridge.app.ui.theme.Mint
import com.airbridge.app.ui.theme.PrimaryCyan
import com.airbridge.app.ui.theme.SurfaceGlass
import com.airbridge.app.ui.theme.TextMuted
import com.airbridge.app.ui.theme.TextPrimary

@Composable
fun SheetHandle(modifier: Modifier = Modifier) {
    Box(
        modifier = modifier
            .padding(vertical = 10.dp)
            .size(width = 44.dp, height = 4.dp)
            .clip(CircleShape)
            .background(Color.White.copy(alpha = 0.18f)),
    )
}

@Composable
fun PeerAvatar(peer: NearbyPeer, size: Dp) {
    val palette = avatarPalette(peer.idHex)
    Box(
        modifier = Modifier.size(size),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(size)
                .clip(CircleShape)
                .background(Brush.linearGradient(palette))
                .border(
                    1.5.dp,
                    if (peer.isTrusted) Mint.copy(alpha = 0.9f) else Color.White.copy(alpha = 0.22f),
                    CircleShape,
                ),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                text = peer.initials,
                color = Color.White,
                fontSize = (size.value * 0.34f).sp,
                fontWeight = FontWeight.SemiBold,
            )
        }
    }
}

// ------------------------------------------------------------------ peer sheet

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PeerSheet(
    peer: NearbyPeer,
    selectedFiles: List<SelectedFile>,
    onDismiss: () -> Unit,
    onSend: () -> Unit,
    onPickFiles: () -> Unit,
    onVerify: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = SurfaceGlass,
        contentColor = TextPrimary,
        scrimColor = Color.Black.copy(alpha = 0.62f),
        dragHandle = { SheetHandle() },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .navigationBarsPadding()
                .padding(bottom = 20.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                PeerAvatar(peer = peer, size = 58.dp)
                Spacer(Modifier.width(14.dp))
                Column(modifier = Modifier.weight(1f)) {
                    Text(
                        text = peer.name,
                        color = TextPrimary,
                        fontSize = 20.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Spacer(Modifier.height(2.dp))
                    Text(
                        text = peer.primaryAddress?.let { "Nearby · $it" } ?: "Nearby device",
                        color = TextMuted,
                        fontSize = 11.5.sp,
                    )
                }
            }

            Spacer(Modifier.height(16.dp))

            if (peer.isTrusted) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        painter = painterResource(R.drawable.ic_check),
                        contentDescription = null,
                        tint = Mint,
                        modifier = Modifier.size(14.dp),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = "Verified device",
                        color = Mint,
                        fontSize = 12.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            } else {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        painter = painterResource(R.drawable.ic_lock),
                        contentDescription = null,
                        tint = Amber,
                        modifier = Modifier.size(14.dp),
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        text = "Not verified — you will compare a code before sending",
                        color = Amber,
                        fontSize = 11.5.sp,
                    )
                }
            }

            Spacer(Modifier.height(18.dp))

            if (selectedFiles.isEmpty()) {
                PrimaryActionButton(
                    text = "Choose files to send",
                    iconRes = R.drawable.ic_add,
                    onClick = onPickFiles,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(10.dp))
                SecondaryActionButton(
                    text = "Verify device",
                    iconRes = R.drawable.ic_lock,
                    onClick = onVerify,
                    modifier = Modifier.fillMaxWidth(),
                )
            } else {
                PrimaryActionButton(
                    text = if (peer.isTrusted) {
                        "Send ${formatCount(selectedFiles.size)}"
                    } else {
                        "Verify & send ${formatCount(selectedFiles.size)}"
                    },
                    iconRes = R.drawable.ic_send,
                    onClick = onSend,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(10.dp))
                SecondaryActionButton(
                    text = "Add more files",
                    iconRes = R.drawable.ic_add,
                    onClick = onPickFiles,
                    modifier = Modifier.fillMaxWidth(),
                )
            }

            Spacer(Modifier.height(14.dp))
            Text(
                text = "ID ${peer.shortId}",
                color = TextMuted.copy(alpha = 0.7f),
                fontSize = 10.sp,
                letterSpacing = 1.5.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }
}

// --------------------------------------------------------------- pairing sheet

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun VerificationSheet(
    peerName: String,
    code: String,
    busy: Boolean,
    onApprove: () -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = { if (!busy) onDismiss() },
        containerColor = SurfaceGlass,
        contentColor = TextPrimary,
        scrimColor = Color.Black.copy(alpha = 0.7f),
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
            Box(
                modifier = Modifier
                    .size(64.dp)
                    .clip(CircleShape)
                    .background(PrimaryCyan.copy(alpha = 0.12f))
                    .border(1.dp, PrimaryCyan.copy(alpha = 0.35f), CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_lock),
                    contentDescription = null,
                    tint = PrimaryCyan,
                    modifier = Modifier.size(26.dp),
                )
            }

            Spacer(Modifier.height(16.dp))
            Text(
                text = "Verify with $peerName",
                color = TextPrimary,
                fontSize = 19.sp,
                fontWeight = FontWeight.SemiBold,
                textAlign = TextAlign.Center,
            )
            Spacer(Modifier.height(6.dp))
            Text(
                text = "Confirm both screens show the same code before you send anything.",
                color = TextMuted,
                fontSize = 12.5.sp,
                textAlign = TextAlign.Center,
                modifier = Modifier.padding(horizontal = 12.dp),
            )

            Spacer(Modifier.height(20.dp))
            SasCode(code)

            Spacer(Modifier.height(16.dp))
            Text(
                text = "The code is derived from both device keys over the encrypted connection.",
                color = TextMuted.copy(alpha = 0.8f),
                fontSize = 11.sp,
                textAlign = TextAlign.Center,
            )

            Spacer(Modifier.height(18.dp))
            if (busy) {
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
                        text = "Verifying…",
                        color = TextPrimary,
                        fontSize = 14.sp,
                        fontWeight = FontWeight.Medium,
                    )
                }
            } else {
                PrimaryActionButton(
                    text = "Codes match",
                    iconRes = R.drawable.ic_check,
                    onClick = onApprove,
                    modifier = Modifier.fillMaxWidth(),
                )
            }

            Spacer(Modifier.height(8.dp))
            TextButton(onClick = onDismiss, enabled = !busy) {
                Text(
                    text = "Cancel",
                    color = TextMuted,
                    fontSize = 13.sp,
                )
            }
        }
    }
}

/** Renders a six-digit SAS as grouped code boxes. */
@Composable
fun SasCode(code: String, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier,
        horizontalArrangement = Arrangement.Center,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        code.filter(Char::isDigit).chunked(3).forEachIndexed { groupIndex, group ->
            group.forEach { digit ->
                CodeBox(digit.toString())
                Spacer(Modifier.width(4.dp))
            }
            if (groupIndex == 0) {
                Spacer(Modifier.width(8.dp))
            }
        }
    }
}

@Composable
private fun CodeBox(digit: String) {
    val shape = RoundedCornerShape(14.dp)
    Box(
        modifier = Modifier
            .size(width = 40.dp, height = 54.dp)
            .clip(shape)
            .background(Color.White.copy(alpha = 0.06f))
            .border(1.dp, PrimaryCyan.copy(alpha = 0.22f), shape),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            text = digit,
            color = TextPrimary,
            fontSize = 23.sp,
            fontWeight = FontWeight.SemiBold,
        )
    }
}

// --------------------------------------------------------------- profile sheet

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileSheet(
    displayName: String,
    deviceId: String,
    nearbyAccess: Boolean,
    onDismiss: () -> Unit,
    onSave: (String) -> Unit,
    onRequestPermissions: () -> Unit,
    onOpenSettings: () -> Unit,
) {
    var name by remember(displayName) { mutableStateOf(displayName) }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = SurfaceGlass,
        contentColor = TextPrimary,
        scrimColor = Color.Black.copy(alpha = 0.62f),
        dragHandle = { SheetHandle() },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .navigationBarsPadding()
                .padding(bottom = 20.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(
                    modifier = Modifier
                        .size(58.dp)
                        .clip(CircleShape)
                        .background(Brush.linearGradient(listOf(Color(0xFF1E7FB8), Color(0xFF0B2A45))))
                        .border(1.5.dp, Color.White.copy(alpha = 0.18f), CircleShape),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        text = displayName.initials(),
                        color = Color.White,
                        fontSize = 20.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                }
                Spacer(Modifier.width(14.dp))
                Column {
                    Text(
                        text = "Your device",
                        color = TextPrimary,
                        fontSize = 20.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Text(
                        text = if (nearbyAccess) "Nearby access granted" else "Nearby access required",
                        color = if (nearbyAccess) Mint else Amber,
                        fontSize = 11.5.sp,
                    )
                }
            }

            Spacer(Modifier.height(20.dp))

            OutlinedTextField(
                value = name,
                onValueChange = { name = it.take(40) },
                label = { Text("Display name") },
                supportingText = {
                    Text(
                        text = "Shown to devices around you",
                        color = TextMuted,
                        fontSize = 11.sp,
                    )
                },
                singleLine = true,
                shape = RoundedCornerShape(16.dp),
                colors = OutlinedTextFieldDefaults.colors(
                    focusedTextColor = TextPrimary,
                    unfocusedTextColor = TextPrimary,
                    focusedBorderColor = PrimaryCyan,
                    unfocusedBorderColor = Color.White.copy(alpha = 0.14f),
                    focusedContainerColor = Color.White.copy(alpha = 0.04f),
                    unfocusedContainerColor = Color.White.copy(alpha = 0.03f),
                    cursorColor = PrimaryCyan,
                    focusedLabelColor = PrimaryCyan,
                    unfocusedLabelColor = TextMuted,
                ),
                modifier = Modifier.fillMaxWidth(),
            )

            Spacer(Modifier.height(14.dp))

            PrimaryActionButton(
                text = "Save name",
                iconRes = R.drawable.ic_check,
                onClick = { onSave(name) },
                modifier = Modifier.fillMaxWidth(),
                enabled = name.trim().isNotEmpty(),
            )

            Spacer(Modifier.height(18.dp))

            GlassSurface(
                modifier = Modifier.fillMaxWidth(),
                shape = RoundedCornerShape(18.dp),
            ) {
                Column(modifier = Modifier.padding(16.dp)) {
                    InfoRow(label = "Device ID", value = deviceId.take(16).uppercase())
                    Spacer(Modifier.height(10.dp))
                    InfoRow(
                        label = "Nearby access",
                        value = if (nearbyAccess) "Granted" else "Not granted",
                        valueColor = if (nearbyAccess) Mint else Amber,
                    )
                }
            }

            Spacer(Modifier.height(12.dp))

            if (!nearbyAccess) {
                SecondaryActionButton(
                    text = "Grant nearby access",
                    iconRes = R.drawable.ic_bluetooth,
                    onClick = onRequestPermissions,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(6.dp))
            }

            TextButton(
                onClick = onOpenSettings,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(
                    text = "Open system settings",
                    color = TextMuted,
                    fontSize = 12.5.sp,
                )
            }
        }
    }
}

@Composable
private fun InfoRow(label: String, value: String, valueColor: Color = TextPrimary) {
    Row(
        modifier = Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = label,
            color = TextMuted,
            fontSize = 12.sp,
            modifier = Modifier.weight(1f),
        )
        Text(
            text = value,
            color = valueColor,
            fontSize = 12.sp,
            fontWeight = FontWeight.Medium,
            letterSpacing = if (label == "Device ID") 1.sp else 0.sp,
        )
    }
}

// ------------------------------------------------------------ permission sheet

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PermissionSheet(
    onDismiss: () -> Unit,
    onAllow: () -> Unit,
) {
    ModalBottomSheet(
        onDismissRequest = onDismiss,
        containerColor = SurfaceGlass,
        contentColor = TextPrimary,
        scrimColor = Color.Black.copy(alpha = 0.7f),
        dragHandle = { SheetHandle() },
    ) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 24.dp)
                .navigationBarsPadding()
                .padding(bottom = 20.dp),
        ) {
            Box(
                modifier = Modifier
                    .size(64.dp)
                    .clip(CircleShape)
                    .background(PrimaryCyan.copy(alpha = 0.12f))
                    .border(1.dp, PrimaryCyan.copy(alpha = 0.35f), CircleShape),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_wifi),
                    contentDescription = null,
                    tint = PrimaryCyan,
                    modifier = Modifier.size(26.dp),
                )
            }

            Spacer(Modifier.height(16.dp))
            Text(
                text = "Let nearby devices find you",
                color = TextPrimary,
                fontSize = 19.sp,
                fontWeight = FontWeight.SemiBold,
            )
            Spacer(Modifier.height(6.dp))
            Text(
                text = "AirBridge uses Bluetooth and your local network to spot people around you. No account, no cloud — files move device to device.",
                color = TextMuted,
                fontSize = 12.5.sp,
            )

            Spacer(Modifier.height(18.dp))
            PermissionRow(
                iconRes = R.drawable.ic_bluetooth,
                title = "Bluetooth",
                subtitle = "Find and be found by nearby devices",
            )
            Spacer(Modifier.height(10.dp))
            PermissionRow(
                iconRes = R.drawable.ic_wifi,
                title = "Local network",
                subtitle = "Transfer directly over Wi-Fi",
            )
            Spacer(Modifier.height(10.dp))
            PermissionRow(
                iconRes = R.drawable.ic_lock,
                title = "Notifications",
                subtitle = "See transfer progress in the background",
            )

            Spacer(Modifier.height(20.dp))
            PrimaryActionButton(
                text = "Allow nearby access",
                iconRes = R.drawable.ic_check,
                onClick = onAllow,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(8.dp))
            TextButton(
                onClick = onDismiss,
                modifier = Modifier.fillMaxWidth(),
            ) {
                Text(
                    text = "Not now",
                    color = TextMuted,
                    fontSize = 13.sp,
                )
            }
        }
    }
}

@Composable
private fun PermissionRow(iconRes: Int, title: String, subtitle: String) {
    GlassSurface(
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(16.dp),
    ) {
        Row(
            modifier = Modifier.padding(14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Box(
                modifier = Modifier
                    .size(38.dp)
                    .clip(RoundedCornerShape(12.dp))
                    .background(PrimaryCyan.copy(alpha = 0.12f)),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    painter = painterResource(iconRes),
                    contentDescription = null,
                    tint = PrimaryCyan,
                    modifier = Modifier.size(18.dp),
                )
            }
            Spacer(Modifier.width(12.dp))
            Column {
                Text(
                    text = title,
                    color = TextPrimary,
                    fontSize = 13.sp,
                    fontWeight = FontWeight.Medium,
                )
                Text(
                    text = subtitle,
                    color = TextMuted,
                    fontSize = 11.sp,
                )
            }
        }
    }
}
