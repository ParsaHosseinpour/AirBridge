package com.airbridge.app.core

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothManager
import android.bluetooth.le.AdvertiseCallback
import android.bluetooth.le.AdvertiseData
import android.bluetooth.le.AdvertiseSettings
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanFilter
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelUuid
import android.util.Log
import androidx.core.content.ContextCompat
import com.airbridge.core.BleBackend
import com.airbridge.core.BleObservationSink
import com.airbridge.core.BleRequest
import com.airbridge.core.FfiException

/**
 * Android BLE tier for the Rust coordinator.
 *
 * Android has no native representation of `BleBackend`, so this class is
 * handed to [com.airbridge.core.PeerDiscoveryEngine.start] as a callback
 * object. The coordinator calls [start], [stop] and [isRunning] from its own
 * worker threads; the radio callbacks arrive on the main looper.
 *
 * The advertised frame is exactly the one the Rust side specifies: the
 * 16-byte peer id followed by the 2-byte big-endian transfer port, carried as
 * service data under `0xFE2C`.
 */
@SuppressLint("MissingPermission")
class AndroidBleBackend(context: Context) : BleBackend {
    private val appContext = context.applicationContext
    private val adapter: BluetoothAdapter? =
        appContext.getSystemService(BluetoothManager::class.java)?.adapter

    private val lock = Any()
    private var sink: BleObservationSink? = null
    private var requestedScan: Boolean = false
    @Volatile private var running: Boolean = false
    @Volatile private var advertisingActive: Boolean = false
    @Volatile private var scanningActive: Boolean = false

    override fun start(request: BleRequest, sink: BleObservationSink) {
        if (!hasBluetoothPermissions(appContext)) {
            throw FfiException.Discovery("Bluetooth permissions have not been granted")
        }
        val radio = adapter ?: throw FfiException.Discovery("This device has no Bluetooth adapter")
        if (!radio.isEnabled) {
            throw FfiException.Discovery("Bluetooth is turned off")
        }
        val frame = encodeFrame(request.peerIdHex, request.port.toInt() and 0xFFFF)
            ?: throw FfiException.Discovery("The discovery engine supplied an invalid peer id")

        synchronized(lock) {
            stopLocked()
            this.sink = sink
            requestedScan = request.scanning
            running = true
            try {
                startAdvertisingLocked(radio, frame)
                if (request.scanning) {
                    startScanningLocked(radio)
                }
            } catch (t: Throwable) {
                stopLocked()
                val reason = t.message?.takeIf { it.isNotBlank() } ?: t.javaClass.simpleName
                throw FfiException.Discovery("BLE backend failed to start: $reason")
            }
        }
    }

    override fun stop() {
        synchronized(lock) {
            stopLocked()
        }
    }

    override fun isRunning(): Boolean {
        val radio = adapter ?: return false
        return try {
            running && radio.isEnabled && advertisingActive && (!requestedScan || scanningActive)
        } catch (_: SecurityException) {
            false
        }
    }

    private fun stopLocked() {
        running = false
        requestedScan = false
        advertisingActive = false
        scanningActive = false
        runCatching { adapter?.bluetoothLeAdvertiser?.stopAdvertising(advertiseCallback) }
        runCatching { adapter?.bluetoothLeScanner?.stopScan(scanCallback) }
        sink = null
    }

    private fun startAdvertisingLocked(radio: BluetoothAdapter, frame: ByteArray) {
        val advertiser = radio.bluetoothLeAdvertiser
            ?: throw IllegalStateException("BLE advertising is unavailable")
        val data = AdvertiseData.Builder()
            .addServiceUuid(SERVICE_UUID)
            .addServiceData(SERVICE_UUID, frame)
            .setIncludeDeviceName(false)
            .setIncludeTxPowerLevel(false)
            .build()
        val settings = AdvertiseSettings.Builder()
            .setAdvertiseMode(AdvertiseSettings.ADVERTISE_MODE_LOW_LATENCY)
            .setTxPowerLevel(AdvertiseSettings.ADVERTISE_TX_POWER_MEDIUM)
            .setConnectable(true)
            .setTimeout(0)
            .build()
        advertisingActive = true
        advertiser.startAdvertising(settings, data, advertiseCallback)
    }

    private fun startScanningLocked(radio: BluetoothAdapter) {
        val scanner = radio.bluetoothLeScanner
            ?: throw IllegalStateException("BLE scanning is unavailable")
        val filter = ScanFilter.Builder()
            .setServiceUuid(SERVICE_UUID)
            .build()
        val settings = ScanSettings.Builder()
            .setScanMode(ScanSettings.SCAN_MODE_LOW_LATENCY)
            .setCallbackType(ScanSettings.CALLBACK_TYPE_ALL_MATCHES)
            .setReportDelay(0L)
            .build()
        scanningActive = true
        scanner.startScan(listOf(filter), settings, scanCallback)
    }

    private val advertiseCallback = object : AdvertiseCallback() {
        override fun onStartSuccess(settingsInEffect: AdvertiseSettings) {
            advertisingActive = true
        }

        override fun onStartFailure(errorCode: Int) {
            advertisingActive = false
            Log.w(TAG, "BLE advertising stopped with error code $errorCode")
        }
    }

    private val scanCallback = object : ScanCallback() {
        override fun onScanResult(callbackType: Int, result: ScanResult) {
            val data = result.scanRecord?.getServiceData(SERVICE_UUID) ?: return
            if (data.size < BEACON_BYTES) return
            val port = ((data[16].toInt() and 0xFF) shl 8) or (data[17].toInt() and 0xFF)
            if (port == 0) return
            val idHex = data.toPeerIdHex() ?: return

            synchronized(lock) {
                val current = sink
                if (!running || current == null) return
                try {
                    current.observe(idHex, port.toUShort())
                } catch (t: Throwable) {
                    // A malformed sighting must never take down the scanner.
                    Log.w(TAG, "Dropping BLE sighting: ${t.message}")
                }
            }
        }

        override fun onScanFailed(errorCode: Int) {
            scanningActive = false
            Log.w(TAG, "BLE scan stopped with error code $errorCode")
        }
    }

    private fun encodeFrame(peerIdHex: String, port: Int): ByteArray? {
        if (peerIdHex.length != 32) return null
        val frame = ByteArray(BEACON_BYTES)
        for (index in 0 until 16) {
            val high = peerIdHex[index * 2].hexValue()
            val low = peerIdHex[index * 2 + 1].hexValue()
            if (high < 0 || low < 0) return null
            frame[index] = ((high shl 4) or low).toByte()
        }
        frame[16] = ((port ushr 8) and 0xFF).toByte()
        frame[17] = (port and 0xFF).toByte()
        return frame
    }

    private fun ByteArray.toPeerIdHex(): String? {
        val builder = StringBuilder(32)
        for (index in 0 until 16) {
            val value = this[index].toInt() and 0xFF
            builder.append(HEX[value ushr 4]).append(HEX[value and 0x0F])
        }
        return builder.toString()
    }

    private fun Char.hexValue(): Int = when (this) {
        in '0'..'9' -> this - '0'
        in 'a'..'f' -> this - 'a' + 10
        in 'A'..'F' -> this - 'A' + 10
        else -> -1
    }

    companion object {
        private const val TAG = "AirBridgeBle"
        private const val BEACON_BYTES = 18
        private const val HEX = "0123456789abcdef"
        private val SERVICE_UUID: ParcelUuid =
            ParcelUuid.fromString("0000fe2c-0000-1000-8000-00805f9b34fb")

        fun hasBluetoothPermissions(context: Context): Boolean =
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_SCAN) ==
                    PackageManager.PERMISSION_GRANTED &&
                    ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_ADVERTISE) ==
                    PackageManager.PERMISSION_GRANTED &&
                    ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_CONNECT) ==
                    PackageManager.PERMISSION_GRANTED
            } else {
                ContextCompat.checkSelfPermission(context, Manifest.permission.ACCESS_FINE_LOCATION) ==
                    PackageManager.PERMISSION_GRANTED
            }
    }
}
