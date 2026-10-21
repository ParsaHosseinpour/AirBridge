using System;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Globalization;
using System.Threading;
using AirBridge.Core;
using AirBridge.Windows.Core;
using Windows.Devices.Bluetooth.Advertisement;
using Windows.Storage.Streams;

namespace AirBridge.Windows.Services;

/// <summary>
/// WinRT implementation of the Rust discovery coordinator's BLE seam.
///
/// The beacon is the same one the Android app publishes: the 16-byte peer id
/// followed by the big-endian transfer port, carried as service data under
/// <c>0xFE2C</c>. Windows requires the 16-bit UUID to be repeated inside the
/// service-data section, so the frame on air is
/// <c>2c fe | id[16] | port[2]</c>. A versioned 19-byte frame from the Rust
/// beacon encoder is accepted as well, so a future binding that advertises the
/// canonical manufacturer-data format is still understood.
///
/// The coordinator calls <see cref="Start"/>, <see cref="Stop"/> and
/// <see cref="IsRunning"/> from Tokio worker threads. The WinRT watcher raises
/// its events on thread-pool threads. All mutable state is guarded by
/// <c>_sync</c>, and <see cref="BleObservationSink.Observe"/> is called while
/// holding it: the Rust side documents that the sink handle is valid only
/// until <see cref="Stop"/> returns, and observing is cheap (it forwards into
/// an unbounded channel).
/// </summary>
public sealed class WindowsBleBackend : BleBackend, IDisposable
{
    private const ushort AirBridgeServiceUuid = 0xFE2C;
    private const int BeaconBytes = 18;
    private const int VersionedBeaconBytes = 19;
    private const int MinimumObservationIntervalMs = 2_500;

    private readonly object _sync = new();
    private readonly ConcurrentDictionary<string, long> _lastObserved = new(StringComparer.OrdinalIgnoreCase);

    private BleObservationSink? _sink;
    private BluetoothLEAdvertisementPublisher? _publisher;
    private BluetoothLEAdvertisementWatcher? _watcher;
    private bool _running;
    private bool _disposed;

    public void Start(BleRequest request, BleObservationSink sink)
    {
        if (request is null)
        {
            throw new ArgumentNullException(nameof(request));
        }

        if (sink is null)
        {
            throw new ArgumentNullException(nameof(sink));
        }

        if (request.ServiceUuid != AirBridgeServiceUuid)
        {
            throw new FfiException.Discovery(
                $"The Windows BLE backend only supports service UUID {AirBridgeServiceUuid:x4}.");
        }

        var frame = EncodeFrame(request.PeerIdHex, request.Port);

        lock (_sync)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            StopLocked();

            try
            {
                var publisher = new BluetoothLEAdvertisementPublisher();
                var section = new BluetoothLEAdvertisementDataSection
                {
                    DataType = BluetoothLEAdvertisementDataTypes.ServiceData16BitUuids,
                    Data = BuildServiceData(frame),
                };
                publisher.Advertisement.DataSections.Add(section);
                publisher.StatusChanged += OnPublisherStatusChanged;

                var watcher = request.Scanning ? CreateWatcher() : null;

                publisher.Start();
                _publisher = publisher;

                if (watcher is not null)
                {
                    watcher.Start();
                    _watcher = watcher;
                }

                _sink = sink;
                _running = true;
                AppLog.Info(
                    $"BLE beacon started (scanning: {request.Scanning}, id {request.PeerIdHex[..8]}\u2026, port {request.Port}).");
            }
            catch (Exception error)
            {
                StopLocked();
                throw new FfiException.Discovery(
                    "The Windows Bluetooth stack could not start advertising: " + Describe(error));
            }
        }
    }

    public void Stop()
    {
        lock (_sync)
        {
            StopLocked();
        }
    }

    public bool IsRunning()
    {
        lock (_sync)
        {
            if (!_running)
            {
                return false;
            }

            try
            {
                var publisherReady = _publisher?.Status == BluetoothLEAdvertisementPublisherStatus.Started;
                var watcherReady = _watcher is null || _watcher.Status == BluetoothLEAdvertisementWatcherStatus.Started;
                return publisherReady && watcherReady;
            }
            catch (Exception error)
            {
                AppLog.Warn("Could not read the Bluetooth radio state.", error);
                return false;
            }
        }
    }

    public void Dispose()
    {
        lock (_sync)
        {
            if (_disposed)
            {
                return;
            }

            StopLocked();
            _disposed = true;
            _lastObserved.Clear();
        }
    }

    private BluetoothLEAdvertisementWatcher CreateWatcher()
    {
        var watcher = new BluetoothLEAdvertisementWatcher
        {
            ScanningMode = BluetoothLEScanningMode.Active,
        };

        // Filtering in the stack keeps unrelated advertisements off the UI
        // thread. The filter is advisory on some adapters, so the received
        // handler validates the service data again.
        watcher.AdvertisementFilter.Advertisement.ServiceUuids.Add(ServiceUuid());
        watcher.Received += OnAdvertisementReceived;
        watcher.Stopped += OnWatcherStopped;
        return watcher;
    }

    private void OnAdvertisementReceived(
        BluetoothLEAdvertisementWatcher sender,
        BluetoothLEAdvertisementReceivedEventArgs args)
    {
        try
        {
            string? peerId = null;
            ushort port = 0;

            foreach (var section in args.Advertisement.GetSectionsByType(
                         BluetoothLEAdvertisementDataTypes.ServiceData16BitUuids))
            {
                if (TryReadBuffer(section.Data, out var bytes) &&
                    TryParseServiceData(bytes, out peerId, out port))
                {
                    break;
                }
            }

            if (peerId is null)
            {
                // Accept the coordinator's canonical 19-byte manufacturer-data
                // beacon too. The first two bytes of manufacturer data are the
                // little-endian company id, which this frame does not use.
                foreach (var manufacturer in args.Advertisement.ManufacturerData)
                {
                    if (TryReadBuffer(manufacturer.Data, out var bytes) &&
                        TryParseManufacturerData(bytes, out peerId, out port))
                    {
                        break;
                    }
                }
            }

            if (peerId is null || port == 0)
            {
                return;
            }

            if (IsDuplicate(peerId))
            {
                return;
            }

            lock (_sync)
            {
                if (!_running || _sink is null)
                {
                    return;
                }

                try
                {
                    _sink.Observe(peerId, port);
                }
                catch (Exception error)
                {
                    // A malformed sighting must never take down the scanner.
                    AppLog.Warn("Dropping a BLE sighting.", error);
                }
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("Ignoring a BLE advertisement that could not be decoded.", error);
        }
    }

    private void OnWatcherStopped(
        BluetoothLEAdvertisementWatcher sender,
        BluetoothLEAdvertisementWatcherStoppedEventArgs args)
    {
        AppLog.WarnEvery(
            $"ble-watcher:{args.Error}",
            TimeSpan.FromMinutes(1),
            $"The Windows BLE scanner stopped ({args.Error}).");
    }

    private void OnPublisherStatusChanged(
        BluetoothLEAdvertisementPublisher sender,
        BluetoothLEAdvertisementPublisherStatusChangedEventArgs args)
    {
        if (args.Status is BluetoothLEAdvertisementPublisherStatus.Aborted or BluetoothLEAdvertisementPublisherStatus.Stopped)
        {
            AppLog.WarnEvery(
                $"ble-publisher:{args.Status}:{args.Error}",
                TimeSpan.FromMinutes(1),
                $"The Windows BLE beacon stopped ({args.Status}, {args.Error}).");
        }
    }

    private bool IsDuplicate(string peerId)
    {
        var now = Stopwatch.GetTimestamp();
        var milliseconds = now * 1000d / Stopwatch.Frequency;

        while (true)
        {
            if (!_lastObserved.TryGetValue(peerId, out var previous))
            {
                if (_lastObserved.TryAdd(peerId, (long)milliseconds))
                {
                    return false;
                }

                continue;
            }

            if (milliseconds - previous < MinimumObservationIntervalMs)
            {
                return true;
            }

            if (_lastObserved.TryUpdate(peerId, (long)milliseconds, previous))
            {
                return false;
            }
        }
    }

    private void StopLocked()
    {
        _running = false;

        var watcher = _watcher;
        _watcher = null;
        if (watcher is not null)
        {
            try
            {
                watcher.Received -= OnAdvertisementReceived;
                watcher.Stopped -= OnWatcherStopped;
                if (watcher.Status is BluetoothLEAdvertisementWatcherStatus.Started or BluetoothLEAdvertisementWatcherStatus.Created)
                {
                    watcher.Stop();
                }
            }
            catch (Exception error)
            {
                AppLog.Warn("The BLE scanner did not stop cleanly.", error);
            }
        }

        var publisher = _publisher;
        _publisher = null;
        if (publisher is not null)
        {
            try
            {
                publisher.StatusChanged -= OnPublisherStatusChanged;
                if (publisher.Status is BluetoothLEAdvertisementPublisherStatus.Started
                    or BluetoothLEAdvertisementPublisherStatus.Waiting
                    or BluetoothLEAdvertisementPublisherStatus.Created)
                {
                    publisher.Stop();
                }
            }
            catch (Exception error)
            {
                AppLog.Warn("The BLE beacon did not stop cleanly.", error);
            }
        }

        // Keep the sink alive until the radio has actually been released: the
        // Rust contract guarantees the handle only until Stop returns.
        _sink = null;
    }

    private static IBuffer BuildServiceData(byte[] frame)
    {
        // A 16-bit service-data AD structure starts with the little-endian
        // UUID, followed by the opaque payload.
        var writer = new DataWriter { ByteOrder = ByteOrder.LittleEndian };
        writer.WriteUInt16(AirBridgeServiceUuid);
        writer.WriteBytes(frame);
        return writer.DetachBuffer();
    }

    private static bool TryReadBuffer(IBuffer? buffer, out byte[] bytes)
    {
        bytes = Array.Empty<byte>();
        if (buffer is null || buffer.Length == 0 || buffer.Length > 512)
        {
            return false;
        }

        try
        {
            bytes = new byte[buffer.Length];
            using var reader = DataReader.FromBuffer(buffer);
            reader.ReadBytes(bytes);
            return true;
        }
        catch
        {
            return false;
        }
    }

    /// <summary>
    /// Parses <c>[uuid:2] id[16] port:2</c> or a bare <c>id[16] port:2</c>.
    /// Also accepts the versioned Rust beacon (<c>01 | id | port</c>).
    /// </summary>
    private static bool TryParseServiceData(byte[] bytes, out string? peerId, out ushort port)
    {
        peerId = null;
        port = 0;

        var offset = 0;
        if (bytes.Length >= 2 &&
            bytes[0] == (AirBridgeServiceUuid & 0xFF) &&
            bytes[1] == (AirBridgeServiceUuid >> 8))
        {
            offset = 2;
        }

        var payload = new ReadOnlySpan<byte>(bytes, offset, bytes.Length - offset);
        return TryParseBeacon(payload, out peerId, out port);
    }

    private static bool TryParseManufacturerData(byte[] bytes, out string? peerId, out ushort port)
    {
        peerId = null;
        port = 0;

        // Skip the company identifier. The canonical AirBridge beacon is
        // 19 bytes and starts with version 1.
        if (bytes.Length < 2 + VersionedBeaconBytes)
        {
            return false;
        }

        var payload = new ReadOnlySpan<byte>(bytes, 2, bytes.Length - 2);
        return TryParseBeacon(payload, out peerId, out port);
    }

    private static bool TryParseBeacon(ReadOnlySpan<byte> payload, out string? peerId, out ushort port)
    {
        peerId = null;
        port = 0;

        if (payload.Length == VersionedBeaconBytes && payload[0] == 1)
        {
            payload = payload[1..];
        }

        if (payload.Length != BeaconBytes)
        {
            return false;
        }

        port = (ushort)((payload[16] << 8) | payload[17]);
        if (port == 0)
        {
            return false;
        }

        peerId = Convert.ToHexString(payload[..16]).ToLowerInvariant();
        return true;
    }

    private static byte[] EncodeFrame(string peerIdHex, ushort port)
    {
        if (peerIdHex is null || peerIdHex.Length != 32)
        {
            throw new FfiException.Discovery("The discovery engine supplied an invalid peer id for BLE advertising.");
        }

        var frame = new byte[BeaconBytes];
        for (var index = 0; index < 16; index++)
        {
            if (!byte.TryParse(
                    peerIdHex.AsSpan(index * 2, 2),
                    NumberStyles.HexNumber,
                    CultureInfo.InvariantCulture,
                    out var value))
            {
                throw new FfiException.Discovery("The discovery engine supplied a non-hexadecimal peer id.");
            }

            frame[index] = value;
        }

        frame[16] = (byte)(port >> 8);
        frame[17] = (byte)(port & 0xFF);
        return frame;
    }

    private static Guid ServiceUuid() =>
        Guid.Parse("0000fe2c-0000-1000-8000-00805f9b34fb");

    private static string Describe(Exception error)
    {
        var text = error switch
        {
            UnauthorizedAccessException => "access to the Bluetooth radio was denied. Check that Bluetooth is turned on.",
            InvalidOperationException invalid => invalid.Message,
            _ => error.Message,
        };

        if (!string.IsNullOrWhiteSpace(text))
        {
            return text.Trim();
        }

        return error.HResult != 0
            ? $"the Bluetooth radio is unavailable (0x{error.HResult:X8})."
            : "the Bluetooth radio is unavailable.";
    }
}
