using System;
using System.Diagnostics;
using AirBridge.Core;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Models;

public enum TransferPhase
{
    Queued,
    Connecting,
    Verifying,
    Running,
    Pausing,
    Paused,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

public enum TransferDirection
{
    Send,
    Receive,
}

/// <summary>
/// One observable transfer card. Mutated only on the UI thread by
/// <see cref="Services.TransferCoordinator"/>.
/// </summary>
public sealed class TransferItem : ObservableObject
{
    private TransferPhase _phase = TransferPhase.Queued;
    private ulong _bytesTransferred;
    private ulong? _totalBytes;
    private ulong? _payloadId;
    private string? _error;
    private string _fileName;
    private string _peerName;
    private string _peerIdHex;
    private string? _peerFingerprint;
    private string? _sas;
    private bool _pairingOnly;
    private string? _destinationPath;
    private double _smoothedRate;
    private long _lastSampleTimestamp;
    private long _lastSampleBytes;
    private readonly Stopwatch _clock = new();

    public TransferItem(
        TransferDirection direction,
        string fileName,
        string sourcePath,
        string peerName,
        string peerIdHex,
        string address)
    {
        Direction = direction;
        _fileName = fileName;
        SourcePath = sourcePath;
        _peerName = string.IsNullOrWhiteSpace(peerName) ? "Nearby device" : peerName;
        _peerIdHex = peerIdHex ?? string.Empty;
        Address = address;
    }

    public Guid Id { get; } = Guid.NewGuid();
    public TransferDirection Direction { get; }
    public string SourcePath { get; }
    public string Address { get; }

    internal FileTransferSession? Session { get; set; }
    internal bool PauseRequested { get; set; }
    internal bool CancelRequested { get; set; }

    public string PeerName
    {
        get => _peerName;
        private set
        {
            if (SetProperty(ref _peerName, value))
            {
                OnPropertiesChanged(nameof(StatusText), nameof(Header), nameof(AutomationName));
            }
        }
    }

    /// <summary>Peer id claimed after the authenticated handshake.</summary>
    public string PeerIdHex
    {
        get => _peerIdHex;
        private set => SetProperty(ref _peerIdHex, value ?? string.Empty);
    }

    /// <summary>SHA-256 fingerprint of the peer's identity key.</summary>
    public string? PeerFingerprint
    {
        get => _peerFingerprint;
        private set => SetProperty(ref _peerFingerprint, value);
    }

    /// <summary>Short authentication string shown while the user compares codes.</summary>
    public string? Sas
    {
        get => _sas;
        private set => SetProperty(ref _sas, value);
    }

    /// <summary>True when this connection verified a device without moving a file.</summary>
    public bool PairingOnly
    {
        get => _pairingOnly;
        private set
        {
            if (SetProperty(ref _pairingOnly, value))
            {
                OnPropertiesChanged(
                    nameof(FileName),
                    nameof(StatusText),
                    nameof(SizeText),
                    nameof(PercentText),
                    nameof(DirectionGlyph),
                    nameof(StatusGlyph),
                    nameof(Header),
                    nameof(AutomationName));
            }
        }
    }

    public string FileName
    {
        get => _fileName;
        private set
        {
            if (SetProperty(ref _fileName, value))
            {
                OnPropertyChanged(nameof(AutomationName));
            }
        }
    }

    public string? DestinationPath
    {
        get => _destinationPath;
        private set => SetProperty(ref _destinationPath, value);
    }

    public TransferPhase Phase
    {
        get => _phase;
        private set
        {
            if (!SetProperty(ref _phase, value))
            {
                return;
            }

            if (value == TransferPhase.Running)
            {
                _clock.Restart();
                _lastSampleTimestamp = Stopwatch.GetTimestamp();
                _lastSampleBytes = (long)Math.Min(_bytesTransferred, long.MaxValue);
                _smoothedRate = 0;
            }

            OnPropertiesChanged(
                nameof(IsActive),
                nameof(IsTerminal),
                nameof(IsPaused),
                nameof(IsCompleted),
                nameof(CanPause),
                nameof(CanResume),
                nameof(CanCancel),
                nameof(CanDismiss),
                nameof(StatusText),
                nameof(PercentText),
                nameof(Progress),
                nameof(RingIsIndeterminate),
                nameof(DirectionGlyph),
                nameof(ActionGlyph),
                nameof(ActionToolTip),
                nameof(StatusGlyph),
                nameof(ProgressColor),
                nameof(AutomationName));
        }
    }

    public ulong BytesTransferred
    {
        get => _bytesTransferred;
        private set => SetProperty(ref _bytesTransferred, value);
    }

    public ulong? TotalBytes
    {
        get => _totalBytes;
        private set
        {
            if (SetProperty(ref _totalBytes, value))
            {
                OnPropertiesChanged(nameof(SizeText), nameof(Progress), nameof(PercentText), nameof(RingIsIndeterminate), nameof(StatusText));
            }
        }
    }

    public ulong? PayloadId
    {
        get => _payloadId;
        private set
        {
            if (SetProperty(ref _payloadId, value))
            {
                OnPropertyChanged(nameof(PayloadIdText));
            }
        }
    }

    public string PayloadIdText => PayloadId is null ? "—" : $"#{PayloadId.Value:x16}";

    public string? Error
    {
        get => _error;
        private set
        {
            if (SetProperty(ref _error, value))
            {
                OnPropertiesChanged(nameof(StatusText), nameof(AutomationName));
            }
        }
    }

    public bool IsTerminal => Phase is TransferPhase.Completed or TransferPhase.Failed or TransferPhase.Cancelled;
    public bool IsActive => !IsTerminal;
    public bool IsPaused => Phase == TransferPhase.Paused;
    public bool IsCompleted => Phase == TransferPhase.Completed;

    public bool CanPause => Direction == TransferDirection.Send &&
                            Phase is TransferPhase.Connecting or TransferPhase.Running;
    public bool CanResume => Direction == TransferDirection.Send && Phase == TransferPhase.Paused;
    public bool CanCancel => Phase is TransferPhase.Queued or TransferPhase.Connecting or TransferPhase.Verifying
        or TransferPhase.Running or TransferPhase.Pausing or TransferPhase.Paused;
    public bool CanDismiss => IsTerminal;

    public double Progress
    {
        get
        {
            if (TotalBytes is not { } total || total == 0)
            {
                return Phase == TransferPhase.Completed ? 100 : 0;
            }

            if (Phase == TransferPhase.Completed)
            {
                return 100;
            }

            return Math.Clamp(BytesTransferred * 100d / total, 0, 100);
        }
    }

    public bool RingIsIndeterminate =>
        Phase is TransferPhase.Connecting or TransferPhase.Verifying or TransferPhase.Pausing or TransferPhase.Cancelling ||
        (Phase is TransferPhase.Queued && TotalBytes is null) ||
        (Phase == TransferPhase.Running && TotalBytes is null);

    public string PercentText => Phase switch
    {
        TransferPhase.Completed => PairingOnly ? "✓" : "100%",
        TransferPhase.Queued => "Queued",
        TransferPhase.Connecting => "…",
        TransferPhase.Verifying => "…",
        TransferPhase.Paused => $"{Progress:0}%",
        TransferPhase.Failed => "!",
        TransferPhase.Cancelled => "—",
        _ => TotalBytes is > 0 ? $"{Progress:0}%" : "…",
    };

    public string SizeText => PairingOnly
        ? "Identity verified"
        : TotalBytes is { } total
            ? $"{Format.Bytes(BytesTransferred)} of {Format.Bytes(total)}"
            : Format.Bytes(BytesTransferred);

    public string StatusText => Phase switch
    {
        TransferPhase.Queued => "Waiting to start",
        TransferPhase.Connecting => Direction == TransferDirection.Send ? "Connecting to device…" : "Waiting for sender…",
        TransferPhase.Verifying => "Compare the security code",
        TransferPhase.Running => TotalBytes is null
            ? (Direction == TransferDirection.Send ? "Sending…" : "Receiving…")
            : $"{Verb} · {Format.Rate(_smoothedRate)}",
        TransferPhase.Pausing => "Pausing…",
        TransferPhase.Paused => $"Paused · {Format.Bytes(BytesTransferred)} saved",
        TransferPhase.Cancelling => "Cancelling…",
        TransferPhase.Completed => PairingOnly
            ? "Device verified"
            : Direction == TransferDirection.Send ? "Sent successfully" : "Received and verified",
        TransferPhase.Failed => Error ?? "Transfer failed",
        TransferPhase.Cancelled => "Cancelled",
        _ => string.Empty,
    };

    public string Verb => Direction == TransferDirection.Send ? "Sending" : "Receiving";

    public string DirectionLabel => Direction == TransferDirection.Send ? "Sending" : "Receiving";

    public string DirectionGlyph => PairingOnly ? "\uE72E" : Direction == TransferDirection.Send ? "\uE898" : "\uE896";

    public string StatusGlyph => Phase switch
    {
        TransferPhase.Completed => PairingOnly ? "\uE72E" : "\uE73E",
        TransferPhase.Verifying => "\uE72E",
        TransferPhase.Failed => "\uE783",
        TransferPhase.Cancelled => "\uE711",
        TransferPhase.Paused => "\uE769",
        _ => DirectionGlyph,
    };

    public string ActionGlyph => Phase == TransferPhase.Paused ? "\uE768" : "\uE769";
    public string ActionToolTip => Phase == TransferPhase.Paused ? "Resume transfer" : "Pause transfer";

    public string ProgressColor => Phase switch
    {
        TransferPhase.Completed => "#FF6EE7C8",
        TransferPhase.Verifying => "#FFFFCF70",
        TransferPhase.Failed => "#FFFF6B7A",
        TransferPhase.Paused => "#FFFFCF70",
        _ => "#FF5AC8FA",
    };

    public string EtaText
    {
        get
        {
            if (TotalBytes is not { } total || total == 0 || _smoothedRate <= 1 || IsTerminal)
            {
                return string.Empty;
            }

            var remaining = total > BytesTransferred ? total - BytesTransferred : 0;
            return Format.Eta(remaining / _smoothedRate);
        }
    }

    public string AutomationName =>
        $"{DirectionLabel} {FileName} {(TotalBytes is null ? string.Empty : $"to {PeerName}")}, {StatusText}";

    public string Header => PairingOnly
        ? "Device verification"
        : Direction == TransferDirection.Send ? $"To {PeerName}" : "Incoming";

    public void SetPayload(ulong? payloadId) => PayloadId = payloadId;

    /// <summary>Updates the authenticated identity of the remote device.</summary>
    public void ApplyPeerIdentity(string peerId, string? fingerprint)
    {
        if (!string.IsNullOrWhiteSpace(peerId))
        {
            PeerIdHex = peerId;
        }

        if (!string.IsNullOrWhiteSpace(fingerprint))
        {
            PeerFingerprint = fingerprint.ToLowerInvariant();
        }
    }

    public void UpdatePeerName(string? name)
    {
        if (!string.IsNullOrWhiteSpace(name))
        {
            PeerName = name;
        }
    }

    public void MarkConnecting()
    {
        if (Phase is TransferPhase.Queued or TransferPhase.Paused)
        {
            _lastSampleTimestamp = 0;
            _lastSampleBytes = (long)Math.Min(BytesTransferred, long.MaxValue);
            _smoothedRate = 0;
            Phase = TransferPhase.Connecting;
        }
    }

    /// <summary>The handshake completed and the user must compare the SAS.</summary>
    public void MarkVerifying(string sas, string fingerprint)
    {
        if (IsTerminal)
        {
            return;
        }

        Sas = sas;
        if (!string.IsNullOrWhiteSpace(fingerprint))
        {
            PeerFingerprint = fingerprint.ToLowerInvariant();
        }

        Error = null;
        Phase = TransferPhase.Verifying;
    }

    public void MarkRunning()
    {
        if (Phase is TransferPhase.Queued or TransferPhase.Connecting or TransferPhase.Verifying or TransferPhase.Paused)
        {
            Sas = null;
            Phase = TransferPhase.Running;
            RaiseLiveMetrics();
        }
    }

    public void MarkPausing()
    {
        if (CanPause)
        {
            Phase = TransferPhase.Pausing;
        }
    }

    public void MarkPaused()
    {
        PauseRequested = false;
        Phase = TransferPhase.Paused;
        OnPropertiesChanged(nameof(StatusText), nameof(CanResume), nameof(CanCancel));
    }

    public void MarkCancelling()
    {
        if (!IsTerminal)
        {
            Phase = TransferPhase.Cancelling;
        }
    }

    public void MarkCancelled()
    {
        CancelRequested = false;
        Sas = null;
        Phase = TransferPhase.Cancelled;
    }

    public void MarkFailed(string message)
    {
        Sas = null;
        Error = message;
        Phase = TransferPhase.Failed;
    }

    public void MarkCompleted(TransferReport report)
    {
        ApplyPeerIdentity(report.PeerId ?? string.Empty, report.PeerFingerprint);
        PairingOnly = report.PairingOnly;
        Sas = null;
        Error = null;

        if (report.PairingOnly)
        {
            FileName = "Device verification";
            PayloadId = null;
            TotalBytes = null;
            BytesTransferred = 0;
            DestinationPath = null;
            _smoothedRate = 0;
            Phase = TransferPhase.Completed;
            RaiseLiveMetrics();
            return;
        }

        PayloadId = report.PayloadId;
        TotalBytes = report.TotalBytes;
        BytesTransferred = report.TotalBytes;
        _smoothedRate = 0;
        DestinationPath = report.DestinationPath;
        Phase = TransferPhase.Completed;
        RaiseLiveMetrics();
    }

    public void ApplyProgress(ulong bytes, ulong? total)
    {
        if (total is not null)
        {
            TotalBytes = total;
        }

        BytesTransferred = bytes;

        var now = Stopwatch.GetTimestamp();
        if (_lastSampleTimestamp == 0)
        {
            _lastSampleTimestamp = now;
            _lastSampleBytes = (long)Math.Min(bytes, long.MaxValue);
        }
        else
        {
            var elapsed = (now - _lastSampleTimestamp) / (double)Stopwatch.Frequency;
            if (elapsed >= 0.25)
            {
                var delta = (long)Math.Min(bytes, long.MaxValue) - _lastSampleBytes;
                var instant = delta / elapsed;
                _smoothedRate = _smoothedRate <= 0 ? instant : (_smoothedRate * 0.62) + (instant * 0.38);
                _lastSampleTimestamp = now;
                _lastSampleBytes = (long)Math.Min(bytes, long.MaxValue);
            }
        }

        RaiseLiveMetrics();
    }

    public void RenameAfterReceive(string fileName, string destinationPath)
    {
        FileName = fileName;
        DestinationPath = destinationPath;
    }

    private void RaiseLiveMetrics() =>
        OnPropertiesChanged(
            nameof(Progress),
            nameof(PercentText),
            nameof(SizeText),
            nameof(StatusText),
            nameof(EtaText),
            nameof(BytesTransferred),
            nameof(RingIsIndeterminate));
}
