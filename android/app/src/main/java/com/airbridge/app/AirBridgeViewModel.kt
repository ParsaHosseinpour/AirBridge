package com.airbridge.app

import android.Manifest
import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.IBinder
import android.provider.OpenableColumns
import androidx.core.content.ContextCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import com.airbridge.app.core.AirBridgeController
import com.airbridge.app.core.AirBridgeService
import com.airbridge.app.core.AppPreferences
import com.airbridge.app.core.ControllerEvent
import com.airbridge.app.core.SasChallenge
import com.airbridge.app.model.DiscoveryUiState
import com.airbridge.app.model.IncomingPhase
import com.airbridge.app.model.IncomingTransferUi
import com.airbridge.app.model.NearbyPeer
import com.airbridge.app.model.SelectedFile
import com.airbridge.app.model.TransferUi
import com.airbridge.app.model.formatCount
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** A pairing sheet that is waiting for the user to compare codes. */
data class PairingUi(
    val peer: NearbyPeer,
    val code: String,
    val challenge: SasChallenge,
    val filesAfterApproval: List<SelectedFile>?,
    val approving: Boolean = false,
)

@OptIn(ExperimentalCoroutinesApi::class)
class AirBridgeViewModel(application: Application) : AndroidViewModel(application) {
    private val app: Application = application
    private val preferences = AppPreferences(application)

    private val _activeController = MutableStateFlow<AirBridgeController?>(null)
    private val _selectedFiles = MutableStateFlow<List<SelectedFile>>(emptyList())
    private val _pairing = MutableStateFlow<PairingUi?>(null)
    private val _peerSheet = MutableStateFlow<NearbyPeer?>(null)
    private val _profileOpen = MutableStateFlow(false)
    private val _permissionSheet = MutableStateFlow(false)
    private val _displayName = MutableStateFlow(preferences.displayName)
    private val _nearbyAccess = MutableStateFlow(nearbyGranted())
    private val _messages = MutableSharedFlow<String>(
        extraBufferCapacity = 8,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    val peers: StateFlow<List<NearbyPeer>> = _activeController
        .flatMapLatest { it?.peers ?: flowOf(emptyList()) }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    val discovery: StateFlow<DiscoveryUiState> = _activeController
        .flatMapLatest { it?.discovery ?: flowOf(DiscoveryUiState(starting = true)) }
        .stateIn(
            viewModelScope,
            SharingStarted.WhileSubscribed(5_000),
            DiscoveryUiState(starting = true),
        )

    val transfer: StateFlow<TransferUi?> = _activeController
        .flatMapLatest { it?.transfer ?: flowOf(null) }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), null)

    val incoming: StateFlow<IncomingTransferUi?> = _activeController
        .flatMapLatest { it?.incoming ?: flowOf(null) }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), null)

    val selectedFiles: StateFlow<List<SelectedFile>> = _selectedFiles.asStateFlow()
    val pairing: StateFlow<PairingUi?> = _pairing.asStateFlow()
    val peerSheet: StateFlow<NearbyPeer?> = _peerSheet.asStateFlow()
    val profileOpen: StateFlow<Boolean> = _profileOpen.asStateFlow()
    val permissionSheet: StateFlow<Boolean> = _permissionSheet.asStateFlow()
    val displayName: StateFlow<String> = _displayName.asStateFlow()
    val nearbyAccess: StateFlow<Boolean> = _nearbyAccess.asStateFlow()
    val messages: SharedFlow<String> = _messages.asSharedFlow()
    val deviceId: String = preferences.peerIdHex

    private var serviceBound = false
    private var bindingInFlight = false
    private var rebindAttempts = 0
    private var eventJob: Job? = null

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            bindingInFlight = false
            serviceBound = true
            rebindAttempts = 0
            val local = service as? AirBridgeService.LocalBinder ?: return
            attach(local.service.controller)
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            handleDisconnect()
        }

        override fun onBindingDied(name: ComponentName?) {
            handleDisconnect()
        }

        override fun onNullBinding(name: ComponentName?) {
            bindingInFlight = false
            handleDisconnect()
        }
    }

    init {
        if (!nearbyGranted()) {
            _permissionSheet.value = true
        }
        startSharingService()
        bindService()
    }

    // ---------------------------------------------------------------- service

    private fun startSharingService() {
        val intent = Intent(app, AirBridgeService::class.java).setAction(AirBridgeService.ACTION_START)
        try {
            ContextCompat.startForegroundService(app, intent)
        } catch (t: Throwable) {
            message("Couldn't start the sharing service")
        }
    }

    private fun bindService() {
        if (serviceBound || bindingInFlight) return
        bindingInFlight = true
        val intent = Intent(app, AirBridgeService::class.java)
        val bound = try {
            app.bindService(intent, connection, Context.BIND_AUTO_CREATE)
        } catch (t: Throwable) {
            false
        }
        if (!bound) {
            bindingInFlight = false
            message("Couldn't connect to the sharing service")
        }
    }

    private fun attach(controller: AirBridgeController) {
        _activeController.value = controller
        eventJob?.cancel()
        eventJob = viewModelScope.launch {
            controller.events.collect { event ->
                when (event) {
                    is ControllerEvent.Message -> _messages.emit(event.text)
                }
            }
        }
    }

    private fun handleDisconnect() {
        val wasBound = serviceBound
        serviceBound = false
        bindingInFlight = false
        eventJob?.cancel()
        _activeController.value = null
        if (wasBound) {
            runCatching { app.unbindService(connection) }
        }
        if (rebindAttempts >= MAX_REBIND_ATTEMPTS) {
            message("Lost the sharing service. Reopen AirBridge to reconnect.")
            return
        }
        rebindAttempts++
        viewModelScope.launch {
            delay(800)
            startSharingService()
            bindService()
        }
    }

    // ---------------------------------------------------------------- files

    fun addFiles(uris: List<Uri>) {
        if (uris.isEmpty()) return
        viewModelScope.launch {
            val described = withContext(Dispatchers.IO) {
                uris.forEach { uri ->
                    runCatching {
                        app.contentResolver.takePersistableUriPermission(
                            uri,
                            Intent.FLAG_GRANT_READ_URI_PERMISSION,
                        )
                    }
                }
                uris.mapNotNull(::describeDocument)
            }
            if (described.isEmpty()) {
                message("Couldn't read the selected files")
                return@launch
            }
            val current = _selectedFiles.value
            val keys = current.mapTo(mutableSetOf()) { it.key }
            val merged = current.toMutableList()
            var added = 0
            described.forEach { file ->
                if (keys.add(file.key)) {
                    merged += file
                    added++
                }
            }
            if (added == 0) {
                message("Already in the list")
                return@launch
            }
            _selectedFiles.value = merged
            message("${formatCount(added)} ready to send")
        }
    }

    fun removeFile(file: SelectedFile) {
        _selectedFiles.value = _selectedFiles.value.filterNot { it.key == file.key }
    }

    fun clearFiles() {
        _selectedFiles.value = emptyList()
    }

    private fun describeDocument(uri: Uri): SelectedFile? = try {
        var name: String? = null
        var size = -1L
        app.contentResolver.query(
            uri,
            arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE),
            null,
            null,
            null,
        )?.use { cursor ->
            if (cursor.moveToFirst()) {
                val nameIndex = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (nameIndex >= 0 && !cursor.isNull(nameIndex)) {
                    name = cursor.getString(nameIndex)
                }
                val sizeIndex = cursor.getColumnIndex(OpenableColumns.SIZE)
                if (sizeIndex >= 0 && !cursor.isNull(sizeIndex)) {
                    size = cursor.getLong(sizeIndex)
                }
            }
        }
        val display = name?.takeIf { it.isNotBlank() }
            ?: uri.lastPathSegment?.substringAfterLast('/')?.takeIf { it.isNotBlank() }
            ?: "File"
        SelectedFile(
            uriString = uri.toString(),
            name = display,
            size = size.coerceAtLeast(0L),
            mimeType = app.contentResolver.getType(uri),
        )
    } catch (t: Throwable) {
        null
    }

    // ---------------------------------------------------------------- sharing

    fun onPeerTap(peer: NearbyPeer) {
        val files = _selectedFiles.value
        if (files.isEmpty()) {
            _peerSheet.value = peer
            return
        }
        if (transfer.value?.phase?.isActive == true) {
            message("Wait for the current transfer to finish")
            return
        }
        routeSend(peer, files)
    }

    fun sendToSheetPeer() {
        val peer = _peerSheet.value ?: return
        val files = _selectedFiles.value
        _peerSheet.value = null
        if (files.isEmpty()) {
            message("Choose files first")
            return
        }
        routeSend(peer, files)
    }

    private fun routeSend(peer: NearbyPeer, files: List<SelectedFile>) {
        val controller = _activeController.value
        if (controller == null) {
            message("The sharing service is still starting up")
            return
        }
        if (peer.isTrusted) {
            controller.send(peer, files)
        } else {
            startPairing(peer, files)
        }
    }

    fun dismissPeerSheet() {
        _peerSheet.value = null
    }

    fun cancelTransfer() {
        _activeController.value?.cancelActiveTransfer()
    }

    fun dismissTransfer() {
        _activeController.value?.dismissTransfer()
    }

    // --------------------------------------------------------------- incoming

    /**
     * True when saving the verified file needs `WRITE_EXTERNAL_STORAGE`.
     *
     * API 29+ writes through MediaStore and needs no runtime grant; API 26-28
     * writes into the public Downloads directory instead.
     */
    fun incomingNeedsStoragePermission(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.Q &&
            incoming.value?.phase == IncomingPhase.READY &&
            !granted(Manifest.permission.WRITE_EXTERNAL_STORAGE)

    fun acceptIncoming() {
        _activeController.value?.acceptIncoming()
    }

    fun declineIncoming() {
        _activeController.value?.declineIncoming()
    }

    fun dismissIncoming() {
        _activeController.value?.dismissIncoming()
    }

    /** Resumes the pending save once the legacy storage prompt is answered. */
    fun onStoragePermissionResult(allowed: Boolean) {
        if (allowed) {
            _activeController.value?.acceptIncoming()
        } else {
            message("Storage access is needed to save received files")
        }
    }

    // ---------------------------------------------------------------- pairing

    fun requestVerification(peer: NearbyPeer) {
        _peerSheet.value = null
        startPairing(peer, emptyList())
    }

    private fun startPairing(peer: NearbyPeer, files: List<SelectedFile>) {
        viewModelScope.launch {
            val controller = _activeController.value ?: return@launch
            try {
                val challenge = controller.createChallenge(peer.idHex)
                _pairing.value = PairingUi(
                    peer = peer,
                    code = challenge.code,
                    challenge = challenge,
                    filesAfterApproval = files.ifEmpty { null },
                )
            } catch (c: CancellationException) {
                throw c
            } catch (t: Throwable) {
                message("Couldn't start verification")
            }
        }
    }

    fun approvePairing() {
        val current = _pairing.value ?: return
        if (current.approving) return
        _pairing.value = current.copy(approving = true)
        viewModelScope.launch {
            val controller = _activeController.value
            val approved = try {
                controller?.approveChallenge(current.challenge) == true
            } catch (c: CancellationException) {
                throw c
            } catch (t: Throwable) {
                false
            }
            if (approved) {
                _pairing.value = null
                val queued = current.filesAfterApproval
                val preview = controller?.isPairingPreview == true
                if (queued.isNullOrEmpty()) {
                    if (preview) {
                        message("Preview PIN accepted — this device is not verified yet")
                    } else {
                        message("${current.peer.name} verified")
                    }
                } else {
                    controller?.send(current.peer, queued)
                }
            } else {
                _pairing.value = current.copy(approving = false)
                message("Verification expired. Try again.")
            }
        }
    }

    fun dismissPairing() {
        if (_pairing.value?.approving == true) return
        _pairing.value = null
    }

    // ---------------------------------------------------------------- sheets

    fun openProfile() {
        _profileOpen.value = true
    }

    fun closeProfile() {
        _profileOpen.value = false
    }

    fun saveDisplayName(name: String) {
        val trimmed = name.trim().take(40)
        if (trimmed.isEmpty()) {
            message("Enter a display name")
            return
        }
        preferences.displayName = trimmed
        _displayName.value = trimmed
        _profileOpen.value = false
        _activeController.value?.updateDisplayName(trimmed)
    }

    fun dismissPermissionSheet() {
        _permissionSheet.value = false
    }

    fun openPermissionSheet() {
        _permissionSheet.value = true
    }

    fun onPermissionsResult() {
        _permissionSheet.value = false
        _nearbyAccess.value = nearbyGranted()
        if (_nearbyAccess.value) {
            startSharingService()
            _activeController.value?.restartDiscovery()
        }
    }

    fun refreshDiscovery() {
        startSharingService()
        val controller = _activeController.value
        if (controller == null) {
            bindService()
        } else {
            controller.restartDiscovery()
        }
    }

    // ---------------------------------------------------------------- helpers

    fun requiredPermissions(): List<String> = buildList {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            add(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            add(Manifest.permission.BLUETOOTH_SCAN)
            add(Manifest.permission.BLUETOOTH_ADVERTISE)
            add(Manifest.permission.BLUETOOTH_CONNECT)
        } else {
            add(Manifest.permission.ACCESS_FINE_LOCATION)
        }
    }

    private fun nearbyGranted(): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            granted(Manifest.permission.BLUETOOTH_SCAN) &&
                granted(Manifest.permission.BLUETOOTH_ADVERTISE) &&
                granted(Manifest.permission.BLUETOOTH_CONNECT)
        } else {
            granted(Manifest.permission.ACCESS_FINE_LOCATION)
        }

    private fun granted(permission: String): Boolean =
        ContextCompat.checkSelfPermission(app, permission) == PackageManager.PERMISSION_GRANTED

    private fun message(text: String) {
        _messages.tryEmit(text)
    }

    override fun onCleared() {
        eventJob?.cancel()
        if (serviceBound) {
            runCatching { app.unbindService(connection) }
            serviceBound = false
        }
        super.onCleared()
    }

    private companion object {
        const val MAX_REBIND_ATTEMPTS = 5
    }
}
