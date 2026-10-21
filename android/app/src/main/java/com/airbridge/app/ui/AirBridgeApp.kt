package com.airbridge.app.ui

import android.Manifest
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Snackbar
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.airbridge.app.AirBridgeViewModel
import com.airbridge.app.model.TransferPhase
import com.airbridge.app.ui.theme.TextPrimary

@Composable
fun AirBridgeApp(viewModel: AirBridgeViewModel) {
    val peers by viewModel.peers.collectAsStateWithLifecycle()
    val discovery by viewModel.discovery.collectAsStateWithLifecycle()
    val transfer by viewModel.transfer.collectAsStateWithLifecycle()
    val incoming by viewModel.incoming.collectAsStateWithLifecycle()
    val selectedFiles by viewModel.selectedFiles.collectAsStateWithLifecycle()
    val pairing by viewModel.pairing.collectAsStateWithLifecycle()
    val peerSheet by viewModel.peerSheet.collectAsStateWithLifecycle()
    val profileOpen by viewModel.profileOpen.collectAsStateWithLifecycle()
    val permissionSheet by viewModel.permissionSheet.collectAsStateWithLifecycle()
    val displayName by viewModel.displayName.collectAsStateWithLifecycle()
    val nearbyAccess by viewModel.nearbyAccess.collectAsStateWithLifecycle()

    val snackbarHostState = remember { SnackbarHostState() }
    val context = LocalContext.current

    val filePicker = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.OpenMultipleDocuments(),
    ) { uris ->
        viewModel.addFiles(uris)
    }

    val permissionLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestMultiplePermissions(),
    ) {
        viewModel.onPermissionsResult()
    }

    val storageLauncher = rememberLauncherForActivityResult(
        contract = ActivityResultContracts.RequestPermission(),
    ) { allowed ->
        viewModel.onStoragePermissionResult(allowed)
    }

    val onPickFiles: () -> Unit = { filePicker.launch(arrayOf("*/*")) }

    LaunchedEffect(Unit) {
        viewModel.messages.collect { message ->
            snackbarHostState.showSnackbar(message)
        }
    }

    Box(modifier = Modifier.fillMaxSize()) {
        AirBridgeBackground()
        Scaffold(
            containerColor = Color.Transparent,
            contentWindowInsets = WindowInsets(0, 0, 0, 0),
            snackbarHost = {
                SnackbarHost(hostState = snackbarHostState) { data ->
                    Snackbar(
                        snackbarData = data,
                        containerColor = Color(0xFF16283C),
                        contentColor = TextPrimary,
                        shape = RoundedCornerShape(14.dp),
                    )
                }
            },
        ) { padding ->
            RadarScreen(
                displayName = displayName,
                peers = peers,
                discovery = discovery,
                transfer = transfer,
                selectedFiles = selectedFiles,
                onPickFiles = onPickFiles,
                onPeerClick = viewModel::onPeerTap,
                onRemoveFile = viewModel::removeFile,
                onClearFiles = viewModel::clearFiles,
                onCancelTransfer = viewModel::cancelTransfer,
                onDismissTransfer = viewModel::dismissTransfer,
                onProfile = viewModel::openProfile,
                onRefresh = viewModel::refreshDiscovery,
                onDropFiles = viewModel::addFiles,
                modifier = Modifier
                    .fillMaxSize()
                    .padding(padding)
                    .safeDrawingPadding(),
            )
        }
    }

    val incomingTransfer = incoming
    if (incomingTransfer != null) {
        IncomingTransferSheet(
            incoming = incomingTransfer,
            onAccept = {
                if (viewModel.incomingNeedsStoragePermission()) {
                    storageLauncher.launch(Manifest.permission.WRITE_EXTERNAL_STORAGE)
                } else {
                    viewModel.acceptIncoming()
                }
            },
            onDecline = viewModel::declineIncoming,
            onDismiss = viewModel::dismissIncoming,
        )
    } else {
        val verifyingTransfer = transfer?.takeIf {
            it.phase == TransferPhase.VERIFYING && it.sas != null
        }
        val currentPairing = pairing
        when {
            currentPairing != null -> VerificationSheet(
                peerName = currentPairing.peer.name,
                code = currentPairing.code,
                busy = currentPairing.approving,
                onApprove = viewModel::approvePairing,
                onDismiss = viewModel::dismissPairing,
            )

            verifyingTransfer != null -> VerificationSheet(
                peerName = verifyingTransfer.peerName,
                code = verifyingTransfer.sas.orEmpty(),
                busy = false,
                onApprove = viewModel::confirmTransferVerification,
                onDismiss = viewModel::rejectTransferVerification,
            )
        }

        peerSheet?.let { peer ->
            PeerSheet(
                peer = peer,
                selectedFiles = selectedFiles,
                onDismiss = viewModel::dismissPeerSheet,
                onSend = viewModel::sendToSheetPeer,
                onPickFiles = {
                    viewModel.dismissPeerSheet()
                    onPickFiles()
                },
                onVerify = { viewModel.requestVerification(peer) },
            )
        }

        if (profileOpen) {
            ProfileSheet(
                displayName = displayName,
                deviceId = viewModel.deviceId,
                nearbyAccess = nearbyAccess,
                onDismiss = viewModel::closeProfile,
                onSave = viewModel::saveDisplayName,
                onRequestPermissions = {
                    permissionLauncher.launch(viewModel.requiredPermissions().toTypedArray())
                },
                onOpenSettings = {
                    val intent = Intent(
                        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.fromParts("package", context.packageName, null),
                    )
                    runCatching { context.startActivity(intent) }
                },
            )
        }

        if (permissionSheet) {
            PermissionSheet(
                onDismiss = viewModel::dismissPermissionSheet,
                onAllow = {
                    permissionLauncher.launch(viewModel.requiredPermissions().toTypedArray())
                },
            )
        }
    }
}
