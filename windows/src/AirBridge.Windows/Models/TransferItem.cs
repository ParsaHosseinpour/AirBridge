using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Linq;
using AirBridge.Core;
using AirBridge.Windows.Core;
using AirBridge.Windows.Services;

namespace AirBridge.Windows.Models;

public enum TransferPhase
{
    /// <summary>Staging the selected files into durable local storage.</summary>
    Preparing,
    Queued,
    Connecting,
    Verifying,
    Running,
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

/// <summary>One child row under a batch transfer card.</summary>
public sealed class TransferFileItem : ObservableObject
{
    private string _relativePath;
    private ulong _size;
    private ulong _bytesTransferred;
    private BatchItemStatus _phase = BatchItemStatus.Pending;
    private string? _error;
    private string? _destinationPath;

    public TransferFileItem(uint index, string relativePath, ulong size)
    {
        Index = index;
        _relativePath = Normalize(relativePath);
        _size = size;
    }

    public uint Index { get; }

    /// <summary>Canonical `/`-separated path from the manifest.</summary>
    public string RelativePath
    {
        get => _relativePath;
        private set
        {
            if (SetProperty(ref _relativePath, Normalize(value)))
            {
                OnPropertiesChanged(nameof(Name), nameof(Folder), nameof(Glyph));
            }
        }
    }

    public string Name => RelativePath.Length == 0
        ? "Unnamed file"
        : RelativePath[(RelativePath.LastIndexOf('/') + 1)..];

    public string? Folder
    {
        get
        {
            var slash = RelativePath.LastIndexOf('/');
            return slash > 0 ? RelativePath[..slash] : null;
        }
    }

    public ulong Size
    {
        get => _size;
        private set
        {
            if (SetProperty(ref _size, value))
            {
                OnPropertiesChanged(nameof(SizeText), nameof(Progress), nameof(PercentText), nameof(StatusText));
            }
        }
    }

    public ulong BytesTransferred
    {
        get => _bytesTransferred;
        private set
        {
            if (SetProperty(ref _bytesTransferred, value))
            {
                OnPropertiesChanged(nameof(Progress), nameof(PercentText), nameof(StatusText), nameof(SizeText));
            }
        }
    }

    public BatchItemStatus Phase
    {
        get => _phase;
        private set
        {
            if (SetProperty(ref _phase, value))
            {
                OnPropertiesChanged(
                    nameof(StatusText),
                    nameof(Progress),
                    nameof(PercentText),
                    nameof(StatusColor),
                    nameof(IsActive),
                    nameof(IsCompleted),
                    nameof(IsFailed));
            }
        }
    }

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

    public string? DestinationPath
    {
        get => _destinationPath;
        private set
        {
            if (SetProperty(ref _destinationPath, value))
            {
                OnPropertyChanged(nameof(AutomationName));
            }
        }
    }

    public bool IsActive => Phase == BatchItemStatus.Active;
    public bool IsCompleted => Phase == BatchItemStatus.Completed;
    public bool IsFailed => Phase == BatchItemStatus.Failed;

    public string Glyph => FileKinds.GlyphFor(Name);
    public string SizeText => Format.Bytes(Size);

    public double Progress
    {
        get
        {
            if (Phase == BatchItemStatus.Completed)
            {
                return 100;
            }

            return Size == 0 ? 0 : Math.Clamp(BytesTransferred * 100d / Size, 0, 100);
        }
    }

    public string PercentText => Phase switch
    {
        BatchItemStatus.Completed => "✓",
        BatchItemStatus.Failed => "!",
        BatchItemStatus.Cancelled => "—",
        BatchItemStatus.Pending => string.Empty,
        _ => Size > 0 ? $"{Progress:0}%" : "…",
    };

    public string StatusText => Phase switch
    {
        BatchItemStatus.Pending => "Waiting",
        BatchItemStatus.Active => Size > 0 ? $"{Progress:0}%" : "…",
        BatchItemStatus.Completed => "Done",
        BatchItemStatus.Failed => string.IsNullOrWhiteSpace(Error) ? "Failed" : Error,
        BatchItemStatus.Cancelled => "Skipped",
        _ => string.Empty,
    };

    public string StatusColor => Phase switch
    {
        BatchItemStatus.Completed => "#FF6EE7C8",
        BatchItemStatus.Failed => "#FFFF6B7A",
        BatchItemStatus.Active => "#FF5AC8FA",
        _ => "#FF9DB2C4",
    };

    public string AutomationName => $"{Name}, {StatusText}";

    public void UpdateIdentity(string relativePath, ulong size)
    {
        if (!string.IsNullOrWhiteSpace(relativePath))
        {
            RelativePath = relativePath;
        }

        if (size > 0)
        {
            Size = size;
        }
    }

    public void MarkActive(ulong size, ulong bytes)
    {
        UpdateIdentity(RelativePath, size);
        Error = null;
        Phase = BatchItemStatus.Active;
        BytesTransferred = Math.Min(bytes, size == 0 ? bytes : size);
    }

    public void MarkCompleted(ulong size, string? destinationPath)
    {
        UpdateIdentity(RelativePath, size);
        Error = null;
        DestinationPath = destinationPath;
        BytesTransferred = size;
        Phase = BatchItemStatus.Completed;
    }

    public void MarkSaved(string destinationPath)
    {
        DestinationPath = destinationPath;
        Error = null;
        BytesTransferred = Size;
        Phase = BatchItemStatus.Completed;
    }

    public void MarkFailed(string? message)
    {
        Error = string.IsNullOrWhiteSpace(message) ? "Failed" : message;
        Phase = BatchItemStatus.Failed;
    }

    public void MarkCancelled()
    {
        if (Phase is BatchItemStatus.Completed or BatchItemStatus.Failed)
        {
            return;
        }

        Phase = BatchItemStatus.Cancelled;
    }

    public void Apply(BatchItemInfo item)
    {
        RelativePath = item.RelativePath;
        Size = item.Size;
        BytesTransferred = item.BytesTransferred;
        DestinationPath = item.DestinationPath;
        Error = item.Error;
        Phase = item.Status;
    }

    private static string Normalize(string? relativePath) =>
        string.IsNullOrWhiteSpace(relativePath)
            ? string.Empty
            : relativePath.Replace('\\', '/').TrimStart('/');
}

/// <summary>
/// One observable transfer card. A send or receive batch is shown as a single
/// card with a per-file child list. Mutated only on the UI thread by
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
    private ulong? _etaMillis;
    private uint _filesCompleted;
    private uint _filesTotal;
    private volatile bool _started;
    private volatile bool _cancelRequested;

    public TransferItem(
        TransferDirection direction,
        string fileName,
        string sourcePath,
        string peerName,
        string peerIdHex,
        string address,
        bool isBatch = false)
    {
        Direction = direction;
        _fileName = fileName;
        SourcePath = sourcePath;
        _peerName = string.IsNullOrWhiteSpace(peerName) ? "Nearby device" : peerName;
        _peerIdHex = peerIdHex ?? string.Empty;
        Address = address;
        IsBatch = isBatch;
    }

    public Guid Id { get; } = Guid.NewGuid();
    public TransferDirection Direction { get; }
    public string SourcePath { get; }
    public string Address { get; }

    /// <summary>True when this card represents a batch manifest, even of one file.</summary>
    public bool IsBatch { get; }

    /// <summary>Per-file rows, in manifest order. Only touched on the UI thread.</summary>
    public ObservableCollection<TransferFileItem> Files { get; } = new();

    internal BatchTransferSession? Session { get; set; }

    internal bool CancelRequested
    {
        get => _cancelRequested;
        set => _cancelRequested = value;
    }

    /// <summary>The local files prepared for an outgoing batch.</summary>
    internal IReadOnlyList<OutgoingFile> Outgoing { get; set; } = Array.Empty<OutgoingFile>();

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

            OnPropertiesChanged(
                nameof(IsActive),
                nameof(IsTerminal),
                nameof(IsCompleted),
                nameof(CanCancel),
                nameof(CanDismiss),
                nameof(StatusText),
                nameof(PercentText),
                nameof(Progress),
                nameof(RingIsIndeterminate),
                nameof(DirectionGlyph),
                nameof(StatusGlyph),
                nameof(ProgressColor),
                nameof(RemainingText),
                nameof(EtaText),
                nameof(AutomationName));
        }
    }

    public ulong BytesTransferred
    {
        get => _bytesTransferred;
        private set
        {
            if (SetProperty(ref _bytesTransferred, value))
            {
                OnPropertiesChanged(nameof(Progress), nameof(PercentText), nameof(SizeText), nameof(StatusText));
            }
        }
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

    public ulong? BatchId { get; private set; }

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
    public bool IsCompleted => Phase == TransferPhase.Completed;

    public bool CanCancel => Phase is TransferPhase.Preparing or TransferPhase.Queued or TransferPhase.Connecting
        or TransferPhase.Verifying or TransferPhase.Running or TransferPhase.Cancelling;
    public bool CanDismiss => IsTerminal;

    public bool HasFileList => IsBatch && Files.Count > 1;

    public uint FilesCompleted => _filesCompleted;
    public uint FilesTotal => _filesTotal;

    public string FileCountText => _filesTotal == 0
        ? string.Empty
        : $"{_filesCompleted} of {_filesTotal} files";

    public int RemainingFiles => Files.Count(file =>
        file.Phase is BatchItemStatus.Pending or BatchItemStatus.Active);

    public string RemainingText
    {
        get
        {
            if (!IsBatch || IsTerminal)
            {
                return string.Empty;
            }

            var remaining = RemainingFiles;
            return remaining switch
            {
                0 => string.Empty,
                1 => "1 file left",
                _ => $"{remaining} files left",
            };
        }
    }

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
        Phase is TransferPhase.Preparing or TransferPhase.Connecting or TransferPhase.Verifying or TransferPhase.Cancelling ||
        (Phase is TransferPhase.Queued && TotalBytes is null) ||
        (Phase == TransferPhase.Running && TotalBytes is null);

    public string PercentText => Phase switch
    {
        TransferPhase.Completed => PairingOnly ? "✓" : "100%",
        TransferPhase.Preparing => "…",
        TransferPhase.Queued => "Queued",
        TransferPhase.Connecting => "…",
        TransferPhase.Verifying => "…",
        TransferPhase.Failed => "!",
        TransferPhase.Cancelled => "—",
        TransferPhase.Cancelling => "…",
        _ => TotalBytes is > 0 ? $"{Progress:0}%" : "…",
    };

    public string SizeText => PairingOnly
        ? "Identity verified"
        : TotalBytes is { } total
            ? $"{Format.Bytes(BytesTransferred)} of {Format.Bytes(total)}"
            : Format.Bytes(BytesTransferred);

    public string StatusText => Phase switch
    {
        TransferPhase.Preparing => "Preparing files…",
        TransferPhase.Queued => "Waiting to start",
        TransferPhase.Connecting => Direction == TransferDirection.Send ? "Connecting to device…" : "Waiting for sender…",
        TransferPhase.Verifying => "Compare the security code",
        TransferPhase.Running => IsBatch && _filesTotal > 0
            ? $"{Verb} · {_filesCompleted} of {_filesTotal} file{(_filesTotal == 1 ? string.Empty : "s")}"
            : Verb,
        TransferPhase.Cancelling => "Cancelling…",
        TransferPhase.Completed => PairingOnly
            ? "Device verified"
            : IsBatch && Files.Count > 0
                ? BuildCompletedText()
                : Direction == TransferDirection.Send ? "Sent successfully" : "Received and verified",
        TransferPhase.Failed => Error ?? "Transfer failed",
        TransferPhase.Cancelled => "Cancelled",
        _ => string.Empty,
    };

    public string Verb => Direction == TransferDirection.Send ? "Sending" : "Receiving";

    public string PastVerb => Direction == TransferDirection.Send ? "Sent" : "Received";

    public string DirectionLabel => Direction == TransferDirection.Send ? "Sending" : "Receiving";

    public string DirectionGlyph => PairingOnly ? "\uE72E" : Direction == TransferDirection.Send ? "\uE898" : "\uE896";

    public string StatusGlyph => Phase switch
    {
        TransferPhase.Completed => PairingOnly ? "\uE72E" : "\uE73E",
        TransferPhase.Verifying => "\uE72E",
        TransferPhase.Failed => "\uE783",
        TransferPhase.Cancelled => "\uE711",
        _ => DirectionGlyph,
    };

    public string ProgressColor => Phase switch
    {
        TransferPhase.Completed => "#FF6EE7C8",
        TransferPhase.Verifying => "#FFFFCF70",
        TransferPhase.Failed => "#FFFF6B7A",
        _ => "#FF5AC8FA",
    };

    /// <summary>Linear estimate supplied by the Rust batch queue.</summary>
    public string EtaText
    {
        get
        {
            if (IsTerminal || _etaMillis is not { } millis || millis == 0)
            {
                return string.Empty;
            }

            return Format.Eta(millis / 1000d);
        }
    }

    public string AutomationName =>
        $"{DirectionLabel} {FileName}, {StatusText}";

    public string Header => PairingOnly
        ? "Device verification"
        : Direction == TransferDirection.Send ? $"To {PeerName}" : "Incoming";

    public bool HandshakeReached =>
        _started ||
        PeerFingerprint is not null ||
        Sas is not null;

    public TransferFileItem AddFile(uint index, string relativePath, ulong size)
    {
        while ((uint)Files.Count <= index)
        {
            Files.Add(new TransferFileItem((uint)Files.Count, string.Empty, 0));
        }

        var row = Files[(int)index];
        row.UpdateIdentity(relativePath, size);
        UpdateCounts();
        RaiseBatchShape();
        return row;
    }

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

    public void MarkPreparing()
    {
        if (Phase == TransferPhase.Queued)
        {
            Phase = TransferPhase.Preparing;
        }
    }

    public void MarkConnecting()
    {
        if (Phase is TransferPhase.Queued or TransferPhase.Preparing)
        {
            Sas = null;
            Error = null;
            _etaMillis = null;
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
        if (Phase is TransferPhase.Queued or TransferPhase.Connecting or TransferPhase.Verifying)
        {
            _started = true;
            Sas = null;
            Phase = TransferPhase.Running;
        }
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
        _etaMillis = null;
        foreach (var file in Files)
        {
            file.MarkCancelled();
        }

        UpdateCounts();
        Phase = TransferPhase.Cancelled;
    }

    public void MarkFailed(string message)
    {
        if (IsTerminal)
        {
            return;
        }

        Sas = null;
        Error = message;
        _etaMillis = null;
        foreach (var file in Files)
        {
            if (file.Phase == BatchItemStatus.Pending)
            {
                file.MarkCancelled();
            }
            else if (file.Phase == BatchItemStatus.Active)
            {
                file.MarkFailed(message);
            }
        }

        UpdateCounts();
        Phase = TransferPhase.Failed;
    }

    /// <summary>Completes the zero-file handshake used by the pairing flow.</summary>
    public void MarkPairingCompleted(string? peerId, string? fingerprint)
    {
        ApplyPeerIdentity(peerId ?? string.Empty, fingerprint);
        PairingOnly = true;
        FileName = "Device verification";
        Sas = null;
        Error = null;
        PayloadId = null;
        BatchId = null;
        TotalBytes = null;
        BytesTransferred = 0;
        DestinationPath = null;
        _etaMillis = null;
        _filesCompleted = 0;
        _filesTotal = 0;
        Files.Clear();
        Phase = TransferPhase.Completed;
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyBatchStarted(ulong batchId, uint totalFiles, ulong totalBytes)
    {
        if (IsTerminal)
        {
            return;
        }

        BatchId = batchId == 0 ? BatchId : batchId;
        _filesTotal = totalFiles > 0 ? totalFiles : _filesTotal;
        if (totalBytes > 0)
        {
            TotalBytes = totalBytes;
        }

        MarkRunning();
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyBatchSnapshot(BatchSnapshot snapshot)
    {
        if (IsTerminal)
        {
            return;
        }

        BatchId = snapshot.BatchId == 0 ? BatchId : snapshot.BatchId;
        _filesTotal = snapshot.TotalFiles > 0 ? snapshot.TotalFiles : _filesTotal;
        _filesCompleted = snapshot.CompletedFiles;
        if (snapshot.TotalBytes > 0)
        {
            TotalBytes = snapshot.TotalBytes;
        }

        BytesTransferred = snapshot.TransferredBytes;
        _etaMillis = snapshot.EstimatedRemainingMs;
        if (snapshot.Phase == BatchPhase.Running)
        {
            MarkRunning();
        }

        if (snapshot.Items.Length > 0)
        {
            SyncRows(snapshot.Items);
            RecountFiles();
        }

        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyBatchProgress(BatchEvent.Progress progress)
    {
        if (IsTerminal)
        {
            return;
        }

        MarkRunning();
        _filesCompleted = progress.FilesCompleted;
        _filesTotal = progress.FilesTotal > 0 ? progress.FilesTotal : _filesTotal;
        BytesTransferred = progress.BytesTransferred;
        if (progress.TotalBytes > 0)
        {
            TotalBytes = progress.TotalBytes;
        }

        _etaMillis = progress.EstimatedRemainingMs;
        if (progress.CurrentFileIndex is { } index)
        {
            var row = EnsureRow(index, string.Empty, progress.FileSize);
            row.MarkActive(progress.FileSize, progress.FileBytesTransferred);
        }

        UpdateCounts();
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyFileStarted(uint index, string relativePath, ulong size)
    {
        if (IsTerminal)
        {
            return;
        }

        MarkRunning();
        var row = EnsureRow(index, relativePath, size);
        row.MarkActive(size, 0);
        UpdateCounts();
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyFileCompleted(uint index, ulong size, string? destinationPath)
    {
        if (IsTerminal)
        {
            return;
        }

        var row = EnsureRow(index, string.Empty, size);
        row.MarkCompleted(size, destinationPath);
        _etaMillis = null;
        UpdateCounts();
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    public void ApplyFileFailed(uint index, string message)
    {
        if (IsTerminal)
        {
            return;
        }

        var row = EnsureRow(index, string.Empty, 0);
        row.MarkFailed(message);
        UpdateCounts();
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    /// <summary>
    /// Applies the terminal report. <paramref name="savedPaths"/> and
    /// <paramref name="saveErrors"/> let a receiver override the staging
    /// destination with the final Downloads path (or a commit failure) before
    /// the card becomes terminal.
    /// </summary>
    public void ApplyBatchReport(
        BatchReport report,
        IReadOnlyDictionary<uint, string>? savedPaths = null,
        IReadOnlyDictionary<uint, string>? saveErrors = null)
    {
        if (IsTerminal)
        {
            return;
        }

        PairingOnly = false;
        BatchId = report.BatchId == 0 ? BatchId : report.BatchId;
        Sas = null;
        Error = null;
        _etaMillis = null;

        if (report.Files.Length > 0)
        {
            SyncRows(report.Files);
            foreach (var item in report.Files)
            {
                if (savedPaths?.TryGetValue(item.Index, out var path) == true)
                {
                    Files[(int)item.Index].MarkSaved(path);
                }
                else if (saveErrors?.TryGetValue(item.Index, out var message) == true)
                {
                    Files[(int)item.Index].MarkFailed(message);
                }
            }

            RecountFiles();
        }

        if (report.TotalBytes > 0)
        {
            TotalBytes = report.TotalBytes;
        }

        if (report.TransferredBytes > 0)
        {
            BytesTransferred = report.TransferredBytes;
        }

        UpdateCounts();
        if (report.Files.Length == 1)
        {
            FileName = Files[0].Name;
        }
        else if (report.Files.Length > 1)
        {
            FileName = $"{report.Files.Length} files";
        }

        DestinationPath = Files
            .Select(file => file.DestinationPath)
            .FirstOrDefault(path => !string.IsNullOrWhiteSpace(path));

        var phase = report.Status switch
        {
            BatchPhase.Cancelled => TransferPhase.Cancelled,
            BatchPhase.Failed => TransferPhase.Failed,
            _ => Files.Any(file => file.Phase == BatchItemStatus.Failed)
                ? TransferPhase.Failed
                : TransferPhase.Completed,
        };

        if (phase == TransferPhase.Cancelled)
        {
            foreach (var file in Files)
            {
                file.MarkCancelled();
            }
        }

        if (phase == TransferPhase.Completed && TotalBytes is { } total && total > 0)
        {
            BytesTransferred = total;
        }

        if (phase == TransferPhase.Failed)
        {
            var completedRows = Files.Count(file => file.Phase == BatchItemStatus.Completed);
            var failedRows = Files.Count(file => file.Phase == BatchItemStatus.Failed);
            Error = completedRows > 0
                ? $"{completedRows} of {Files.Count} files {PastVerb.ToLowerInvariant()}" +
                  (failedRows > 0 ? $"; {failedRows} failed" : string.Empty)
                : Files.FirstOrDefault(file => file.Phase == BatchItemStatus.Failed)?.Error
                    ?? "The transfer failed";
        }

        UpdateCounts();
        Phase = phase;
        RaiseBatchShape();
        RaiseLiveMetrics();
    }

    private void SyncRows(IReadOnlyList<BatchItemInfo> items)
    {
        if (Files.Count != items.Count)
        {
            Files.Clear();
            foreach (var item in items)
            {
                Files.Add(new TransferFileItem(item.Index, item.RelativePath, item.Size));
            }
        }

        for (var index = 0; index < items.Count; index++)
        {
            Files[index].Apply(items[index]);
        }
    }

    private TransferFileItem EnsureRow(uint index, string relativePath, ulong size)
    {
        if ((uint)Files.Count <= index)
        {
            AddFile(index, relativePath, size);
        }
        else
        {
            Files[(int)index].UpdateIdentity(relativePath, size);
        }

        return Files[(int)index];
    }

    private void UpdateCounts()
    {
        if (Files.Count > 0)
        {
            _filesTotal = Math.Max(_filesTotal, (uint)Files.Count);
            _filesCompleted = Math.Max(
                _filesCompleted,
                (uint)Files.Count(file => file.Phase == BatchItemStatus.Completed));
        }
    }

    private void RecountFiles()
    {
        if (Files.Count == 0)
        {
            return;
        }

        _filesTotal = (uint)Files.Count;
        _filesCompleted = (uint)Files.Count(file => file.Phase == BatchItemStatus.Completed);
    }

    private string BuildCompletedText()
    {
        var completed = Files.Count(file => file.Phase == BatchItemStatus.Completed);
        if (completed >= Files.Count)
        {
            return $"{PastVerb} {Files.Count} file{(Files.Count == 1 ? string.Empty : "s")}";
        }

        return $"{PastVerb} {completed} of {Files.Count} files";
    }

    private void RaiseBatchShape() =>
        OnPropertiesChanged(
            nameof(HasFileList),
            nameof(Files),
            nameof(FilesCompleted),
            nameof(FilesTotal),
            nameof(FileCountText),
            nameof(RemainingFiles),
            nameof(RemainingText),
            nameof(StatusText),
            nameof(FileName),
            nameof(AutomationName));

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
