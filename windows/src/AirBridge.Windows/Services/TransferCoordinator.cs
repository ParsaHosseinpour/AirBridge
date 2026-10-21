using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.IO;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using AirBridge.Core;
using AirBridge.Windows.Core;
using AirBridge.Windows.Models;
using Microsoft.UI.Dispatching;
using TransferDirection = AirBridge.Windows.Models.TransferDirection;

namespace AirBridge.Windows.Services;

public enum NoticeSeverity
{
    Informational,
    Success,
    Warning,
    Error,
}

public sealed class TransferNotice : EventArgs
{
    public TransferNotice(string message, NoticeSeverity severity)
    {
        Message = message;
        Severity = severity;
    }

    public string Message { get; }
    public NoticeSeverity Severity { get; }
}

/// <summary>An authenticated id-to-key binding learned from a handshake.</summary>
public sealed class PeerVerifiedEventArgs : EventArgs
{
    public PeerVerifiedEventArgs(string peerId, string peerFingerprint)
    {
        PeerId = peerId;
        PeerFingerprint = peerFingerprint;
    }

    public string PeerId { get; }
    public string PeerFingerprint { get; }
}

/// <summary>A file ready to leave the device.</summary>
public sealed class OutgoingFile
{
    public OutgoingFile(string path, string name, long size, string relativePath)
    {
        Path = path;
        Name = name;
        Size = size;
        RelativePath = string.IsNullOrWhiteSpace(relativePath)
            ? name
            : relativePath.Replace('\\', '/');
    }

    public string Path { get; }
    public string Name { get; }
    public long Size { get; }

    /// <summary>Safe `/`-separated path carried in the batch manifest.</summary>
    public string RelativePath { get; }
}

/// <summary>
/// Owns every <see cref="BatchTransferSession"/>: a serial send queue and a
/// long-lived receive listener.
///
/// Sends and receives are always batch sessions, even for a single file, so
/// both ends share one manifest/progress protocol. A batch is one card with a
/// per-file child list. The Rust engine has no pause primitive; cancellation
/// stops the batch and preserves the receiver's part files for a resume with
/// the same batch id.
///
/// Incoming batches are written into a private staging directory and are only
/// moved into Downloads after the manifest has been verified. Relative folder
/// paths are preserved.
/// </summary>
public sealed class TransferCoordinator : IAsyncDisposable
{
    private const int AcceptTimeoutMs = 120_000;
    private const int ResponseTimeoutMs = 90_000;
    private const int ReceiverRetryDelayMs = 500;
    private const int ReceiverErrorDelayMs = 5_000;
    private const int ConnectRetryDelayMs = 600;
    private const int MaxConnectRetries = 4;
    private const string StagingDirectoryName = ".airbridge-staging";

    private readonly DispatcherQueue _dispatcher;
    private readonly Func<AppSettings> _settings;
    private readonly Func<byte[]?> _identityPkcs8;
    private readonly Func<string, string?>? _peerNameLookup;
    private readonly SemaphoreSlim _sendSignal = new(0);
    private readonly ConcurrentQueue<TransferItem> _sendQueue = new();
    private readonly object _gate = new();

    private CancellationTokenSource? _lifetime;
    private Task? _sendWorker;
    private CancellationTokenSource? _receiverCts;
    private Task? _receiverTask;
    private BatchTransferSession? _activeReceiveSession;
    private string? _lastReceiverError;
    private bool _disposed;

    public TransferCoordinator(
        DispatcherQueue dispatcher,
        Func<AppSettings> settings,
        Func<byte[]?> identityPkcs8,
        Func<string, string?>? peerNameLookup = null)
    {
        _dispatcher = dispatcher ?? throw new ArgumentNullException(nameof(dispatcher));
        _settings = settings ?? throw new ArgumentNullException(nameof(settings));
        _identityPkcs8 = identityPkcs8 ?? throw new ArgumentNullException(nameof(identityPkcs8));
        _peerNameLookup = peerNameLookup;
    }

    /// <summary>All transfers, newest first. Only touched on the UI thread.</summary>
    public ObservableCollection<TransferItem> Transfers { get; } = new();

    /// <summary>True while an inbound listener is bound.</summary>
    public bool IsReceiverListening { get; private set; }

    /// <summary>
    /// Shows the SAS dialog and returns the user's decision. Verification is
    /// required before the encrypted channel is allowed to move payload bytes.
    /// </summary>
    public Func<SasPrompt, Task<bool>>? VerificationPrompt { get; set; }

    public event EventHandler<TransferNotice>? Notice;
    public event EventHandler<PeerVerifiedEventArgs>? PeerVerified;
    public event EventHandler? ReceiverStateChanged;

    public void Start(AppSettings settings)
    {
        lock (_gate)
        {
            if (_disposed || _lifetime is not null)
            {
                return;
            }

            _lifetime = new CancellationTokenSource();
            _sendWorker = Task.Run(() => SendWorkerAsync(_lifetime.Token));
            _receiverTask = Task.Run(() => ReceiveLoopAsync(_lifetime.Token));
        }
    }

    /// <summary>Queues one batch containing every supplied file.</summary>
    public void EnqueueSend(PeerDevice peer, IReadOnlyList<OutgoingFile> files)
    {
        ArgumentNullException.ThrowIfNull(peer);
        ArgumentNullException.ThrowIfNull(files);

        if (files.Count == 0)
        {
            return;
        }

        Post(() =>
        {
            var item = new TransferItem(
                TransferDirection.Send,
                files.Count == 1 ? files[0].Name : $"{files.Count} files",
                files[0].Path,
                peer.Name,
                peer.IdHex,
                peer.PrimaryAddress,
                isBatch: true);

            item.Outgoing = files;
            for (var index = 0; index < files.Count; index++)
            {
                item.AddFile(
                    (uint)index,
                    files[index].RelativePath,
                    (ulong)Math.Max(0, files[index].Size));
            }

            item.MarkPreparing();
            Transfers.Insert(0, item);
            _sendQueue.Enqueue(item);
            _sendSignal.Release();
        });
    }

    public void Cancel(TransferItem item)
    {
        if (item is null || item.CanDismiss)
        {
            return;
        }

        item.CancelRequested = true;
        Post(item.MarkCancelling);

        try
        {
            item.Session?.Cancel();
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not cancel the transfer session.", error);
        }
    }

    public void CancelAll()
    {
        foreach (var item in Transfers.ToArray())
        {
            Cancel(item);
        }
    }

    public void Dismiss(TransferItem item)
    {
        if (item is null || !item.CanDismiss)
        {
            return;
        }

        Post(() => Transfers.Remove(item));
    }

    public void ClearFinished()
    {
        Post(() =>
        {
            foreach (var item in Transfers.Where(transfer => transfer.IsTerminal).ToArray())
            {
                Transfers.Remove(item);
            }
        });
    }

    /// <summary>Rebuilds the inbound listener, e.g. after a port change.</summary>
    public async Task RestartReceiverAsync()
    {
        CancellationTokenSource? cts;
        Task? task;
        BatchTransferSession? session;

        lock (_gate)
        {
            cts = _receiverCts;
            task = _receiverTask;
        }

        try
        {
            cts?.Cancel();
        }
        catch
        {
            // Already disposed.
        }

        lock (_gate)
        {
            session = _activeReceiveSession;
        }

        try
        {
            session?.Cancel();
        }
        catch
        {
            // The session may already have finished.
        }

        if (task is not null)
        {
            try
            {
                await task.ConfigureAwait(false);
            }
            catch (Exception error)
            {
                AppLog.Warn("The inbound listener did not stop cleanly.", error);
            }
        }

        SetReceiverListening(false);

        lock (_gate)
        {
            if (_disposed || _lifetime is null)
            {
                return;
            }

            _receiverCts?.Dispose();
            _receiverCts = CancellationTokenSource.CreateLinkedTokenSource(_lifetime.Token);
            _receiverTask = Task.Run(() => ReceiveLoopAsync(_receiverCts.Token));
        }
    }

    private async Task SendWorkerAsync(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            try
            {
                await _sendSignal.WaitAsync(token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }
            catch (ObjectDisposedException)
            {
                return;
            }

            while (_sendQueue.TryDequeue(out var item))
            {
                try
                {
                    await RunSendAsync(item, token).ConfigureAwait(false);
                }
                catch (OperationCanceledException)
                {
                    return;
                }
                catch (Exception error)
                {
                    AppLog.Error("An unexpected error ended a send.", error);
                    Post(() => item.MarkFailed(Format.FriendlyException(error)));
                }
            }
        }
    }

    private async Task RunSendAsync(TransferItem item, CancellationToken token)
    {
        if (item.CancelRequested)
        {
            Post(item.MarkCancelled);
            return;
        }

        var connectAttempts = 0;
        ulong? batchId = null;

        while (!token.IsCancellationRequested)
        {
            Post(item.MarkConnecting);

            BatchTransferSession? session = null;
            BatchTransferEvents? events = null;
            var retry = false;

            try
            {
                var settings = _settings();
                var identity = _identityPkcs8();
                if (identity is null || identity.Length == 0)
                {
                    throw new FfiException.InvalidArgument(
                        "This device's identity is not ready yet. Restart AirBridge and try again.");
                }

                var files = item.Outgoing;
                session = BatchTransferSession.Send(
                    item.Address,
                    new BatchOptions(
                        IdentityPkcs8: identity,
                        TrustedPeerFingerprints: settings.TrustedFingerprints.ToArray(),
                        BatchId: batchId,
                        ResponseTimeoutMs: ResponseTimeoutMs,
                        ProgressIntervalMs: 100,
                        LocalPeerId: settings.PeerIdHex ?? string.Empty,
                        ExpectedPeerFingerprint: settings.PeerFingerprint(item.PeerIdHex)));

                batchId = session.BatchId() ?? batchId;

                var sessionReference = session;
                Post(() => item.Session = sessionReference);

                foreach (var file in files)
                {
                    if (item.CancelRequested)
                    {
                        session.Cancel();
                        break;
                    }

                    await session.AddFile(file.Path, file.RelativePath).ConfigureAwait(false);
                }

                events = new BatchTransferEvents(sessionReference);
                events.EventReceived += (_, transferEvent) =>
                    OnBatchEvent(item, sessionReference, transferEvent);
                events.Start();

                var report = await sessionReference.Run().ConfigureAwait(false);
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyBatchReport(report);
                    }
                });

                NotifyBatchResult(item, report);
                return;
            }
            catch (OperationCanceledException)
            {
                Post(item.MarkCancelled);
                return;
            }
            catch (Exception error)
            {
                if (item.CancelRequested || error is FfiException.Cancelled)
                {
                    Post(item.MarkCancelled);
                    return;
                }

                // Pairing and transfers share one listener port. Give a
                // receiver that is re-arming after a pairing connection a few
                // chances before failing the card. Never retry once a code was
                // shown or bytes moved.
                if (error is FfiException.Io &&
                    connectAttempts < MaxConnectRetries &&
                    !item.HandshakeReached)
                {
                    connectAttempts++;
                    AppLog.Info(
                        $"Retrying connection to {item.Address} ({connectAttempts}/{MaxConnectRetries}) after a refused connection.");
                    retry = true;
                }
                else
                {
                    Post(() => item.MarkFailed(Format.FriendlyException(error)));
                    Notify(
                        $"Could not send to {item.PeerName}: {Format.FriendlyException(error)}",
                        NoticeSeverity.Error);
                    return;
                }
            }
            finally
            {
                if (events is not null)
                {
                    try
                    {
                        // The stream is already exhausted at this point, so
                        // waiting cannot block on a running transfer.
                        await events.DisposeAsync().ConfigureAwait(false);
                    }
                    catch (Exception error)
                    {
                        AppLog.Warn("The transfer event pump did not close cleanly.", error);
                    }
                }

                Post(() => item.Session = null);
                session?.Dispose();
            }

            if (!retry)
            {
                return;
            }

            try
            {
                await Task.Delay(ConnectRetryDelayMs, token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                return;
            }
        }

        Post(item.MarkCancelled);
    }

    private void NotifyBatchResult(TransferItem item, BatchReport report)
    {
        if (report.Status == BatchPhase.Cancelled)
        {
            Notify("Transfer cancelled.", NoticeSeverity.Informational);
            return;
        }

        if (report.Status == BatchPhase.Failed || report.FailedFiles > 0)
        {
            var verb = item.Direction == TransferDirection.Send ? "sent" : "received";
            Notify(
                $"{report.CompletedFiles} of {report.TotalFiles} files {verb}; {report.FailedFiles} failed.",
                report.CompletedFiles > 0 ? NoticeSeverity.Warning : NoticeSeverity.Error);
            return;
        }

        var summary = report.TotalFiles == 1 ? item.FileName : $"{report.TotalFiles} files";
        Notify(
            item.Direction == TransferDirection.Send
                ? $"Sent {summary} to {item.PeerName}."
                : $"Received {summary}.",
            NoticeSeverity.Success);
    }

    private void OnBatchEvent(
        TransferItem item,
        BatchTransferSession session,
        BatchEvent transferEvent)
    {
        switch (transferEvent)
        {
            case BatchEvent.VerificationRequired verification:
                Post(() => item.MarkVerifying(verification.Sas, verification.PeerFingerprint));
                BeginVerification(item, session, verification);
                break;
            case BatchEvent.PeerVerified verified:
                Post(() =>
                {
                    item.ApplyPeerIdentity(verified.PeerId, verified.PeerFingerprint);
                    if (_peerNameLookup?.Invoke(verified.PeerId) is { Length: > 0 } name)
                    {
                        item.UpdatePeerName(name);
                    }
                });
                PeerVerified?.Invoke(
                    this,
                    new PeerVerifiedEventArgs(verified.PeerId, verified.PeerFingerprint));
                break;
            case BatchEvent.Started started:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyBatchStarted(started.BatchId, started.TotalFiles, started.TotalBytes);
                    }
                });
                break;
            case BatchEvent.FileStarted fileStarted:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyFileStarted(fileStarted.Index, fileStarted.RelativePath, fileStarted.Size);
                    }
                });
                break;
            case BatchEvent.Progress progress:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyBatchProgress(progress);
                    }
                });
                break;
            case BatchEvent.FileCompleted fileCompleted:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyFileCompleted(fileCompleted.Index, fileCompleted.Size, fileCompleted.DestinationPath);
                    }
                });
                break;
            case BatchEvent.FileFailed fileFailed:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyFileFailed(
                            fileFailed.Index,
                            string.IsNullOrWhiteSpace(fileFailed.Message) ? fileFailed.Kind : fileFailed.Message);
                    }
                });
                break;
            case BatchEvent.Finished:
            case BatchEvent.Cancelled:
                // Run() resolves with the authoritative report; the send loop
                // applies it exactly once.
                break;
            case BatchEvent.Failed failed:
                // Run() surfaces the same failure; its catch decides whether
                // the connection can be retried before the card is terminal.
                AppLog.Warn($"The outgoing batch failed: {failed.Kind} {failed.Message}");
                break;
            case BatchEvent.Lagged:
            {
                var snapshot = session.Snapshot();
                Post(() => item.ApplyBatchSnapshot(snapshot));
                break;
            }
        }
    }

    /// <summary>
    /// Asks the user to compare the SAS, then releases or drops the encrypted
    /// channel. The event pump is not blocked: the FFI waits for the decision.
    /// </summary>
    private void BeginVerification(
        TransferItem item,
        BatchTransferSession session,
        BatchEvent.VerificationRequired verification)
    {
        _ = ConfirmVerificationAsync(item, session, verification);
    }

    private async Task ConfirmVerificationAsync(
        TransferItem item,
        BatchTransferSession session,
        BatchEvent.VerificationRequired verification)
    {
        try
        {
            var settings = _settings();
            var knownPeerId = settings.PeerIdForFingerprint(verification.PeerFingerprint);
            var knownName = knownPeerId is null ? null : _peerNameLookup?.Invoke(knownPeerId);
            var isSend = item.Direction == TransferDirection.Send;

            var prompt = new SasPrompt(
                verification.Sas,
                verification.PeerFingerprint,
                knownName ?? (isSend ? item.PeerName : "Nearby device"),
                isSend ? $"{item.PeerName} \u00B7 {item.Address}" : "Incoming connection",
                isIncoming: !isSend,
                glyph: isSend ? "\uE8D4" : "\uE7BA");

            var approved = false;
            var handler = VerificationPrompt;
            if (handler is not null)
            {
                try
                {
                    approved = await handler(prompt).ConfigureAwait(false);
                }
                catch (Exception error)
                {
                    AppLog.Error("The verification prompt failed.", error);
                    approved = false;
                }
            }

            if (item.IsTerminal)
            {
                return;
            }

            if (approved)
            {
                session.ConfirmPeer();
            }
            else
            {
                session.RejectPeer();
            }
        }
        catch (Exception error)
        {
            // The session may have timed out or been cancelled while the
            // dialog was open. Its own failure path updates the card.
            AppLog.Warn("The verification decision could not be delivered.", error);
        }
    }

    private async Task ReceiveLoopAsync(CancellationToken token)
    {
        var retryDelay = ReceiverRetryDelayMs;
        var stagingCleaned = false;

        while (!token.IsCancellationRequested)
        {
            var settings = _settings();
            if (!settings.ReceiveEnabled)
            {
                SetReceiverListening(false);
                if (!await DelayAsync(1_500, token).ConfigureAwait(false))
                {
                    return;
                }

                continue;
            }

            if (!stagingCleaned)
            {
                CleanupStaleStaging(settings.DownloadDirectory);
                stagingCleaned = true;
            }

            BatchTransferSession? session = null;
            BatchTransferEvents? events = null;
            string? stagingForCleanup = null;

            try
            {
                Directory.CreateDirectory(settings.DownloadDirectory);

                var stagingRoot = Path.Combine(settings.DownloadDirectory, StagingDirectoryName);
                Directory.CreateDirectory(stagingRoot);

                var stagingPath = Path.Combine(stagingRoot, Guid.NewGuid().ToString("N"));
                Directory.CreateDirectory(stagingPath);
                stagingForCleanup = stagingPath;

                var identity = _identityPkcs8();
                if (identity is null || identity.Length == 0)
                {
                    throw new FfiException.InvalidArgument(
                        "This device's identity is not ready yet.");
                }

                session = BatchTransferSession.Receive(
                    $"0.0.0.0:{settings.ListenPort}",
                    stagingPath,
                    new BatchOptions(
                        IdentityPkcs8: identity,
                        TrustedPeerFingerprints: settings.TrustedFingerprints.ToArray(),
                        AckEveryBytes: 4 * 1024 * 1024,
                        VerifyWholeFile: true,
                        ResponseTimeoutMs: ResponseTimeoutMs,
                        ProgressIntervalMs: 100,
                        AcceptTimeoutMs: AcceptTimeoutMs,
                        LocalPeerId: settings.PeerIdHex ?? string.Empty));

                var receiveItem = new TransferItem(
                    TransferDirection.Receive,
                    "Incoming files",
                    stagingPath,
                    "Nearby device",
                    string.Empty,
                    $"port {settings.ListenPort}",
                    isBatch: true);

                var sessionReference = session;
                var announcement = new ReceiverAnnouncement();

                lock (_gate)
                {
                    _activeReceiveSession = sessionReference;
                }

                SetReceiverListening(true);
                retryDelay = ReceiverRetryDelayMs;

                events = new BatchTransferEvents(sessionReference);
                events.EventReceived += (_, transferEvent) =>
                    OnReceiveEvent(receiveItem, sessionReference, transferEvent, announcement);
                events.Start();

                BatchReport? report = null;
                try
                {
                    report = await sessionReference.Run().ConfigureAwait(false);
                }
                catch (FfiException.Timeout)
                {
                    // Nobody connected within the accept window; rebind.
                }
                catch (FfiException.Cancelled)
                {
                    // Either the user cancelled a transfer in flight or the
                    // listener is being restarted.
                    if (announcement.Announced || IsAuthenticated(sessionReference))
                    {
                        Post(() =>
                        {
                            EnsureVisible(receiveItem);
                            receiveItem.MarkCancelled();
                        });
                    }
                }
                catch (Exception error)
                {
                    if (announcement.Announced || IsAuthenticated(sessionReference))
                    {
                        Post(() =>
                        {
                            EnsureVisible(receiveItem);
                            receiveItem.MarkFailed(Format.FriendlyException(error));
                        });
                    }
                    else
                    {
                        AppLog.Warn("The inbound listener failed before accepting a transfer.", error);
                    }
                }

                if (report is not null)
                {
                    CompleteReceive(receiveItem, sessionReference, stagingPath, report, settings);
                }
            }
            catch (Exception error) when (error is not OperationCanceledException)
            {
                var message = Format.FriendlyException(error);
                if (!string.Equals(message, _lastReceiverError, StringComparison.Ordinal))
                {
                    _lastReceiverError = message;
                    Notify(
                        $"Incoming transfers are unavailable: {message}",
                        NoticeSeverity.Warning);
                }

                SetReceiverListening(false);
                retryDelay = ReceiverErrorDelayMs;
            }
            finally
            {
                if (events is not null)
                {
                    try
                    {
                        await events.DisposeAsync().ConfigureAwait(false);
                    }
                    catch (Exception error)
                    {
                        AppLog.Warn("The receive event pump did not close cleanly.", error);
                    }
                }

                lock (_gate)
                {
                    _activeReceiveSession = null;
                }

                session?.Dispose();

                if (stagingForCleanup is not null)
                {
                    TryDeleteDirectory(stagingForCleanup);
                }
            }

            if (!await DelayAsync(retryDelay, token).ConfigureAwait(false))
            {
                return;
            }
        }

        SetReceiverListening(false);
    }

    private void OnReceiveEvent(
        TransferItem item,
        BatchTransferSession session,
        BatchEvent transferEvent,
        ReceiverAnnouncement announcement)
    {
        switch (transferEvent)
        {
            case BatchEvent.VerificationRequired verification:
                // The handshake is complete, so this is a real AirBridge peer.
                // Show the card while the user compares the SAS.
                announcement.Announced = true;
                Post(() =>
                {
                    EnsureVisible(item);
                    item.MarkVerifying(verification.Sas, verification.PeerFingerprint);
                });
                BeginVerification(item, session, verification);
                break;
            case BatchEvent.PeerVerified verified:
                // The handshake completed, so a later failure is real and
                // should be visible even if the manifest never started.
                announcement.Announced = true;
                Post(() =>
                {
                    EnsureVisible(item);
                    item.ApplyPeerIdentity(verified.PeerId, verified.PeerFingerprint);
                    if (_peerNameLookup?.Invoke(verified.PeerId) is { Length: > 0 } name)
                    {
                        item.UpdatePeerName(name);
                    }
                });
                PeerVerified?.Invoke(
                    this,
                    new PeerVerifiedEventArgs(verified.PeerId, verified.PeerFingerprint));
                break;
            case BatchEvent.Started started:
            {
                // The manifest is accepted. The snapshot contains every file,
                // so the card can show the full batch before the first byte.
                announcement.Announced = true;
                var startedSnapshot = session.Snapshot();
                Post(() =>
                {
                    EnsureVisible(item);
                    item.ApplyBatchSnapshot(startedSnapshot);
                    item.ApplyBatchStarted(started.BatchId, started.TotalFiles, started.TotalBytes);
                });
                break;
            }
            case BatchEvent.FileStarted fileStarted:
                announcement.Announced = true;
                Post(() =>
                {
                    EnsureVisible(item);
                    item.ApplyFileStarted(fileStarted.Index, fileStarted.RelativePath, fileStarted.Size);
                });
                break;
            case BatchEvent.Progress progress:
                if (!announcement.Announced &&
                    (progress.FilesTotal > 0 || progress.TotalBytes > 0))
                {
                    announcement.Announced = true;
                    Post(() => EnsureVisible(item));
                }

                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyBatchProgress(progress);
                    }
                });
                break;
            case BatchEvent.FileCompleted fileCompleted:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyFileCompleted(
                            fileCompleted.Index,
                            fileCompleted.Size,
                            fileCompleted.DestinationPath);
                    }
                });
                break;
            case BatchEvent.FileFailed fileFailed:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.ApplyFileFailed(
                            fileFailed.Index,
                            string.IsNullOrWhiteSpace(fileFailed.Message) ? fileFailed.Kind : fileFailed.Message);
                    }
                });
                break;
            case BatchEvent.Finished:
                // The authoritative report is committed after Run() returns.
                break;
            case BatchEvent.Failed failed:
                if (!announcement.Announced && !IsAuthenticated(session))
                {
                    // A port scan or another local service connected and sent
                    // bytes that are not an AirBridge frame. Do not turn that
                    // noise into a failed transfer in the UI.
                    AppLog.Warn(
                        $"Ignored an invalid inbound connection: {failed.Kind} {failed.Message}");
                    break;
                }

                Post(() =>
                {
                    EnsureVisible(item);
                    if (!item.IsTerminal)
                    {
                        item.MarkFailed(string.IsNullOrWhiteSpace(failed.Message) ? failed.Kind : failed.Message);
                    }
                });
                break;
            case BatchEvent.Cancelled:
                break;
            case BatchEvent.Lagged:
            {
                var snapshot = session.Snapshot();
                if (!announcement.Announced && snapshot.TotalFiles > 0)
                {
                    announcement.Announced = true;
                    Post(() => EnsureVisible(item));
                }

                Post(() => item.ApplyBatchSnapshot(snapshot));
                break;
            }
        }
    }

    private void CompleteReceive(
        TransferItem item,
        BatchTransferSession session,
        string staging,
        BatchReport report,
        AppSettings settings)
    {
        if (report.TotalFiles == 0 && report.Files.Length == 0 && report.CompletedFiles == 0)
        {
            // Pairing and transfers share the listener port. A pairing-only
            // connection carries no manifest and is a normal outcome.
            var peerId = session.PeerId();
            var fingerprint = session.PeerFingerprint();
            Post(() =>
            {
                EnsureVisible(item);
                item.MarkPairingCompleted(peerId, fingerprint);
            });

            _lastReceiverError = null;
            Notify("Device verified.", NoticeSeverity.Success);
            _ = DismissPairingCardAsync(item);
            return;
        }

        if (report.Status == BatchPhase.Cancelled)
        {
            Post(() =>
            {
                EnsureVisible(item);
                item.ApplyBatchReport(report);
            });

            _lastReceiverError = null;
            Notify("Incoming transfer cancelled.", NoticeSeverity.Informational);
            return;
        }

        BatchCommitResult commit;
        try
        {
            commit = CommitReceivedBatch(staging, report, settings);
        }
        catch (Exception error)
        {
            AppLog.Error("The received batch could not be committed.", error);
            Post(() =>
            {
                EnsureVisible(item);
                item.MarkFailed(Format.FriendlyException(error));
            });
            Notify("Incoming transfer failed.", NoticeSeverity.Error);
            return;
        }

        Post(() =>
        {
            EnsureVisible(item);
            item.ApplyBatchReport(report, commit.SavedPaths, commit.Errors);
        });

        _lastReceiverError = null;

        var saved = commit.SavedPaths.Count;
        var failed = report.FailedFiles + commit.Errors.Count;

        if (saved == 0)
        {
            Notify("Incoming transfer failed.", NoticeSeverity.Error);
        }
        else if (failed > 0)
        {
            Notify(
                $"Received {saved} of {report.TotalFiles} files; {failed} failed.",
                NoticeSeverity.Warning);
        }
        else
        {
            var first = commit.SavedPaths.Values.FirstOrDefault();
            var summary = saved == 1 && first is not null
                ? Path.GetFileName(first)
                : $"{saved} files";
            Notify($"Received {summary}.", NoticeSeverity.Success);
        }
    }

    private void EnsureVisible(TransferItem item)
    {
        if (!Transfers.Contains(item))
        {
            Transfers.Insert(0, item);
        }
    }

    /// <summary>
    /// True once the peer completed the authenticated handshake. The event
    /// pump can lag behind <c>Run()</c>, so failure paths also consult the
    /// session state before deciding that a connection was just noise.
    /// </summary>
    private static bool IsAuthenticated(BatchTransferSession session)
    {
        try
        {
            return !string.IsNullOrWhiteSpace(session.PeerFingerprint());
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not read the inbound peer identity.", error);
            return false;
        }
    }

    private async Task DismissPairingCardAsync(TransferItem item)
    {
        await Task.Delay(TimeSpan.FromSeconds(6)).ConfigureAwait(false);
        Post(() =>
        {
            if (item.PairingOnly && item.IsTerminal)
            {
                Transfers.Remove(item);
            }
        });
    }

    private static BatchCommitResult CommitReceivedBatch(
        string staging,
        BatchReport report,
        AppSettings settings)
    {
        var result = new BatchCommitResult();

        foreach (var item in report.Files)
        {
            if (item.Status != BatchItemStatus.Completed)
            {
                continue;
            }

            try
            {
                var source = ResolveStagedFile(staging, item);
                var segments = SafeSegments(item.RelativePath);

                var directory = settings.DownloadDirectory;
                for (var index = 0; index < segments.Count - 1; index++)
                {
                    directory = Path.Combine(directory, segments[index]);
                }

                Directory.CreateDirectory(directory);

                var fileName = segments[^1];
                var stem = Path.GetFileNameWithoutExtension(fileName);
                var extension = Path.GetExtension(fileName);
                if (string.IsNullOrEmpty(stem))
                {
                    stem = fileName;
                    extension = string.Empty;
                }

                var target = FileKinds.UniquePath(directory, stem, extension);
                File.Move(source, target);
                result.SavedPaths[item.Index] = target;
            }
            catch (Exception error)
            {
                result.Errors[item.Index] = Format.FriendlyException(error);
                AppLog.Warn(
                    $"Could not commit the received file {item.RelativePath}.",
                    error);
            }
        }

        return result;
    }

    /// <summary>Finds a verified file inside the private staging tree.</summary>
    private static string ResolveStagedFile(string staging, BatchItemInfo item)
    {
        var root = Path.GetFullPath(staging);

        if (!string.IsNullOrWhiteSpace(item.DestinationPath))
        {
            var reported = Path.GetFullPath(item.DestinationPath);
            if (IsInside(root, reported) && File.Exists(reported))
            {
                return reported;
            }
        }

        var candidate = root;
        foreach (var segment in SplitRelativePath(item.RelativePath))
        {
            candidate = Path.Combine(candidate, segment);
        }

        candidate = Path.GetFullPath(candidate);
        if (!IsInside(root, candidate) || !File.Exists(candidate))
        {
            throw new IOException("The received file is no longer available.");
        }

        return candidate;
    }

    /// <summary>
    /// Splits a manifest path and applies the second traversal guard at the
    /// storage boundary. The Rust receiver already validated the components,
    /// but the commit code must not trust a path just because it arrived from
    /// the network.
    /// </summary>
    private static IReadOnlyList<string> SafeSegments(string relativePath)
    {
        var segments = SplitRelativePath(relativePath);
        if (segments.Count == 0 || segments.Count > 64)
        {
            throw new IOException("The received file path is not safe.");
        }

        var sanitized = new string[segments.Count];
        for (var index = 0; index < segments.Count; index++)
        {
            var name = Format.SanitizeFileName(segments[index], "file");
            if (IsReservedDeviceName(name))
            {
                name = "_" + name;
            }

            sanitized[index] = name;
        }

        return sanitized;
    }

    private static IReadOnlyList<string> SplitRelativePath(string? relativePath)
    {
        var parts = (relativePath ?? string.Empty)
            .Replace('\\', '/')
            .Split('/', StringSplitOptions.RemoveEmptyEntries);

        foreach (var part in parts)
        {
            if (part is "." or ".." || part.Contains(':'))
            {
                throw new IOException("The sender used an unsafe file path.");
            }
        }

        return parts;
    }

    private static bool IsReservedDeviceName(string name)
    {
        var stem = Path.GetFileNameWithoutExtension(name).ToUpperInvariant();
        return stem is "CON" or "PRN" or "AUX" or "NUL" ||
               stem.Length == 4 && stem[..3] is "COM" or "LPT" && stem[3] is >= '1' and <= '9';
    }

    private static bool IsInside(string root, string candidate)
    {
        var prefix = root.TrimEnd(Path.DirectorySeparatorChar, Path.AltDirectorySeparatorChar) +
                     Path.DirectorySeparatorChar;
        return string.Equals(root, candidate, StringComparison.OrdinalIgnoreCase) ||
               candidate.StartsWith(prefix, StringComparison.OrdinalIgnoreCase);
    }

    private static void CleanupStaleStaging(string downloadDirectory)
    {
        try
        {
            var root = Path.Combine(downloadDirectory, StagingDirectoryName);
            if (!Directory.Exists(root))
            {
                return;
            }

            foreach (var directory in Directory.EnumerateDirectories(root))
            {
                TryDeleteDirectory(directory);
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not clean the inbound staging area.", error);
        }
    }

    private static void TryDeleteDirectory(string path)
    {
        try
        {
            if (Directory.Exists(path))
            {
                Directory.Delete(path, true);
            }
        }
        catch (Exception error)
        {
            AppLog.Warn($"The staging directory {path} could not be removed.", error);
        }
    }

    private void SetReceiverListening(bool listening)
    {
        if (IsReceiverListening == listening)
        {
            return;
        }

        IsReceiverListening = listening;
        ReceiverStateChanged?.Invoke(this, EventArgs.Empty);
    }

    private void Notify(string message, NoticeSeverity severity) =>
        Notice?.Invoke(this, new TransferNotice(message, severity));

    private void Post(Action action)
    {
        if (_dispatcher.HasThreadAccess)
        {
            action();
        }
        else
        {
            _dispatcher.TryEnqueue(() => action());
        }
    }

    private static async Task<bool> DelayAsync(int milliseconds, CancellationToken token)
    {
        try
        {
            await Task.Delay(milliseconds, token).ConfigureAwait(false);
            return true;
        }
        catch (OperationCanceledException)
        {
            return false;
        }
    }

    public async ValueTask DisposeAsync()
    {
        CancellationTokenSource? lifetime;
        Task? sender;
        Task? receiver;
        BatchTransferSession? session;

        lock (_gate)
        {
            if (_disposed)
            {
                return;
            }

            _disposed = true;
            lifetime = _lifetime;
            _lifetime = null;
            sender = _sendWorker;
            _sendWorker = null;
            receiver = _receiverTask;
            _receiverTask = null;
            session = _activeReceiveSession;
        }

        try
        {
            lifetime?.Cancel();
        }
        catch
        {
            // Already disposed.
        }

        try
        {
            session?.Cancel();
        }
        catch
        {
            // Already finished.
        }

        foreach (var item in Transfers.ToArray())
        {
            try
            {
                item.Session?.Cancel();
            }
            catch
            {
                // Best effort on shutdown.
            }
        }

        foreach (var task in new[] { sender, receiver })
        {
            if (task is null)
            {
                continue;
            }

            try
            {
                await task.ConfigureAwait(false);
            }
            catch (Exception error)
            {
                AppLog.Warn("A transfer worker did not stop cleanly.", error);
            }
        }

        lifetime?.Dispose();
        _sendSignal.Dispose();
    }

    private sealed class ReceiverAnnouncement
    {
        public volatile bool Announced;
    }

    private sealed class BatchCommitResult
    {
        public Dictionary<uint, string> SavedPaths { get; } = new();
        public Dictionary<uint, string> Errors { get; } = new();
    }
}
