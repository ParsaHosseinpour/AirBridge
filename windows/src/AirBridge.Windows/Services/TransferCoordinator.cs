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
    public OutgoingFile(string path, string name, long size)
    {
        Path = path;
        Name = name;
        Size = size;
    }

    public string Path { get; }
    public string Name { get; }
    public long Size { get; }
}

/// <summary>
/// Owns every <see cref="FileTransferSession"/>: a serial send queue and a
/// long-lived receive listener.
///
/// The Rust engine runs one session per payload. Sends are therefore queued and
/// executed one at a time, exactly like the Android frontend. Pause is not an
/// engine primitive: a pause cancels the running session, the receiver's part
/// file survives, and resuming builds a new session with the same payload id
/// so the sender seeks to the receiver's verified prefix instead of
/// restarting.
///
/// Incoming transfers carry no filename and no peer identity on the wire, so a
/// received file is committed under a generated name and then renamed with a
/// sniffed extension once the digest has been verified.
/// </summary>
public sealed class TransferCoordinator : IAsyncDisposable
{
    private const int AcceptTimeoutMs = 120_000;
    private const int ReceiverRetryDelayMs = 500;
    private const int ReceiverErrorDelayMs = 5_000;

    private static readonly TimeSpan ProgressInterval = TimeSpan.FromMilliseconds(100);

    private readonly DispatcherQueue _dispatcher;
    private readonly Func<AppSettings> _settings;
    private readonly Func<byte[]?> _identityPkcs8;
    private readonly Func<string, string?>? _peerNameLookup;
    private readonly SemaphoreSlim _sendSignal = new(0);
    private readonly ConcurrentQueue<TransferItem> _sendQueue = new();
    private readonly object _gate = new();
    private readonly object _resumeGate = new();

    private CancellationTokenSource? _lifetime;
    private Task? _sendWorker;
    private CancellationTokenSource? _receiverCts;
    private Task? _receiverTask;
    private FileTransferSession? _activeReceiveSession;
    private TaskCompletionSource<bool>? _resumeSignal;
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

    /// <summary>Queues one send job per file. The files are sent in order.</summary>
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
            foreach (var file in files)
            {
                var item = new TransferItem(
                    TransferDirection.Send,
                    file.Name,
                    file.Path,
                    peer.Name,
                    peer.IdHex,
                    peer.PrimaryAddress);

                Transfers.Insert(0, item);
                _sendQueue.Enqueue(item);
            }
        });

        _sendSignal.Release(files.Count);
    }

    public void Pause(TransferItem item)
    {
        if (item is null || !item.CanPause)
        {
            return;
        }

        item.PauseRequested = true;
        Post(item.MarkPausing);

        try
        {
            item.Session?.Cancel();
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not pause the transfer session.", error);
        }
    }

    public void Resume(TransferItem item)
    {
        if (item is null || !item.CanResume)
        {
            return;
        }

        item.PauseRequested = false;
        item.CancelRequested = false;
        Post(item.MarkConnecting);

        lock (_resumeGate)
        {
            _resumeSignal?.TrySetResult(true);
        }
    }

    public void Cancel(TransferItem item)
    {
        if (item is null || item.CanDismiss)
        {
            return;
        }

        item.CancelRequested = true;
        item.PauseRequested = false;
        Post(item.MarkCancelling);

        lock (_resumeGate)
        {
            _resumeSignal?.TrySetResult(false);
        }

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
        FileTransferSession? session;

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

        // Pairing and the file transfer use the same listener port. The
        // receiver needs a moment to re-arm after a pairing-only connection, so
        // retry a refused connection a few times before failing the card.
        var connectAttempts = 0;

        while (!token.IsCancellationRequested)
        {
            Post(item.MarkConnecting);

            FileTransferSession? session = null;
            FileTransferEvents? events = null;

            try
            {
                var settings = _settings();
                var identity = _identityPkcs8();
                if (identity is null || identity.Length == 0)
                {
                    throw new FfiException.InvalidArgument(
                        "This device's identity is not ready yet. Restart AirBridge and try again.");
                }

                session = FileTransferSession.Send(
                    item.Address,
                    item.SourcePath,
                    new SendOptions(
                        IdentityPkcs8: identity,
                        TrustedPeerFingerprints: settings.TrustedFingerprints.ToArray(),
                        PayloadId: item.PayloadId,
                        ResponseTimeoutMs: 90_000,
                        ProgressIntervalMs: 100,
                        LocalPeerId: settings.PeerIdHex ?? string.Empty,
                        ExpectedPeerFingerprint: settings.PeerFingerprint(item.PeerIdHex)));
            }
            catch (Exception error)
            {
                if (item.CancelRequested)
                {
                    Post(item.MarkCancelled);
                }
                else
                {
                    Post(() => item.MarkFailed(Format.FriendlyException(error)));
                }

                return;
            }

            try
            {
                var sessionReference = session;
                Post(() =>
                {
                    item.Session = sessionReference;
                    item.SetPayload(sessionReference.PayloadId());
                });

                if (item.PauseRequested)
                {
                    // Pause arrived while the session was being created.
                    session.Cancel();
                }

                events = new FileTransferEvents(session);
                events.EventReceived += (_, transferEvent) => OnTransferEvent(item, sessionReference, transferEvent);
                events.Start();

                var report = await session.Run().ConfigureAwait(false);

                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.MarkCompleted(report);
                    }
                });

                Notify(
                    item.Direction == TransferDirection.Send
                        ? $"Sent {item.FileName} to {item.PeerName}."
                        : $"Received {item.FileName}.",
                    NoticeSeverity.Success);
                return;
            }
            catch (Exception error)
            {
                if (item.CancelRequested)
                {
                    Post(item.MarkCancelled);
                    return;
                }

                if (item.PauseRequested && error is FfiException.Cancelled)
                {
                    Post(item.MarkPaused);
                    var resumed = await WaitForResumeAsync(token).ConfigureAwait(false);
                    if (resumed && !item.CancelRequested && !token.IsCancellationRequested)
                    {
                        continue;
                    }

                    Post(item.MarkCancelled);
                    return;
                }

                if (error is FfiException.Cancelled)
                {
                    Post(item.MarkCancelled);
                    return;
                }

                if (!item.CancelRequested &&
                    item.Phase is TransferPhase.Queued or TransferPhase.Connecting &&
                    connectAttempts < 4 &&
                    error is FfiException.Io)
                {
                    connectAttempts++;
                    AppLog.Info(
                        $"Retrying connection to {item.Address} ({connectAttempts}/4) after a refused connection.");
                    try
                    {
                        await Task.Delay(600, token).ConfigureAwait(false);
                    }
                    catch (OperationCanceledException)
                    {
                        Post(item.MarkCancelled);
                        return;
                    }

                    continue;
                }

                Post(() => item.MarkFailed(Format.FriendlyException(error)));
                return;
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
                session.Dispose();
            }
        }
    }

    private Task<bool> WaitForResumeAsync(CancellationToken token)
    {
        TaskCompletionSource<bool> signal;
        lock (_resumeGate)
        {
            signal = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
            _resumeSignal = signal;
        }

        token.Register(() => signal.TrySetResult(false));
        return signal.Task;
    }

    private void OnTransferEvent(
        TransferItem item,
        FileTransferSession session,
        TransferEvent transferEvent)
    {
        switch (transferEvent)
        {
            case TransferEvent.VerificationRequired verification:
                Post(() => item.MarkVerifying(verification.Sas, verification.PeerFingerprint));
                BeginVerification(item, session, verification);
                break;
            case TransferEvent.PeerVerified verified:
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
            case TransferEvent.Started started:
                Post(() =>
                {
                    if (item.IsTerminal)
                    {
                        return;
                    }

                    item.MarkRunning();
                    if (started.TotalBytes is { } total)
                    {
                        item.ApplyProgress(item.BytesTransferred, total);
                    }
                });
                break;
            case TransferEvent.Progress progress:
                Post(() =>
                {
                    if (item.IsTerminal)
                    {
                        return;
                    }

                    item.MarkRunning();
                    item.ApplyProgress(progress.BytesTransferred, progress.TotalBytes);
                });
                break;
            case TransferEvent.Completed completed:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.MarkCompleted(completed.Report);
                    }
                });
                break;
            case TransferEvent.Failed failed:
                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.MarkFailed(string.IsNullOrWhiteSpace(failed.Message) ? failed.Kind : failed.Message);
                    }
                });
                break;
            case TransferEvent.Cancelled:
            case TransferEvent.Lagged:
                // Cancellation is resolved by the owning send/receive loop so
                // a pause is not mistaken for a user cancel.
                break;
        }
    }

    /// <summary>
    /// Asks the user to compare the SAS, then releases or drops the encrypted
    /// channel. The event pump is not blocked: the FFI waits for the decision.
    /// </summary>
    private void BeginVerification(
        TransferItem item,
        FileTransferSession session,
        TransferEvent.VerificationRequired verification)
    {
        _ = ConfirmVerificationAsync(item, session, verification);
    }

    private async Task ConfirmVerificationAsync(
        TransferItem item,
        FileTransferSession session,
        TransferEvent.VerificationRequired verification)
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

            FileTransferSession? session = null;
            FileTransferEvents? events = null;
            TransferItem? item = null;
            var announced = false;

            try
            {
                Directory.CreateDirectory(settings.DownloadDirectory);

                var destination = Path.Combine(
                    settings.DownloadDirectory,
                    $".airbridge-incoming-{Guid.NewGuid():N}.bin");

                var identity = _identityPkcs8();
                if (identity is null || identity.Length == 0)
                {
                    throw new FfiException.InvalidArgument(
                        "This device's identity is not ready yet.");
                }

                session = FileTransferSession.Receive(
                    $"0.0.0.0:{settings.ListenPort}",
                    destination,
                    new ReceiveOptions(
                        IdentityPkcs8: identity,
                        TrustedPeerFingerprints: settings.TrustedFingerprints.ToArray(),
                        ExpectedPayloadId: null,
                        AckEveryBytes: 4 * 1024 * 1024,
                        VerifyWholeFile: true,
                        AcceptTimeoutMs: AcceptTimeoutMs,
                        ProgressIntervalMs: 100,
                        LocalPeerId: settings.PeerIdHex ?? string.Empty));

                var receiveItem = new TransferItem(
                    TransferDirection.Receive,
                    "Incoming file",
                    destination,
                    "Nearby device",
                    string.Empty,
                    $"port {settings.ListenPort}");

                item = receiveItem;
                receiveItem.Session = session;

                lock (_gate)
                {
                    _activeReceiveSession = session;
                }

                SetReceiverListening(true);
                retryDelay = ReceiverRetryDelayMs;

                events = new FileTransferEvents(session);
                events.EventReceived += (_, transferEvent) =>
                    OnReceiveEvent(receiveItem, session, transferEvent, settings, ref announced);
                events.Start();

                try
                {
                    await session.Run().ConfigureAwait(false);
                }
                catch (FfiException.Timeout)
                {
                    // Nobody connected within the accept window; rebind.
                }
                catch (FfiException.Cancelled)
                {
                    // Either the user cancelled a transfer in flight or the
                    // listener is being restarted.
                }
                catch (Exception error)
                {
                    if (announced)
                    {
                        Post(() => receiveItem.MarkFailed(Format.FriendlyException(error)));
                    }
                    else
                    {
                        AppLog.Warn("The inbound listener failed before accepting a transfer.", error);
                    }
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
        FileTransferSession session,
        TransferEvent transferEvent,
        AppSettings settings,
        ref bool announced)
    {
        switch (transferEvent)
        {
            case TransferEvent.VerificationRequired verification:
                // The handshake is complete, so this is a real AirBridge peer.
                // Show the card while the user compares the SAS.
                announced = true;
                Post(() =>
                {
                    if (!Transfers.Contains(item))
                    {
                        Transfers.Insert(0, item);
                    }

                    item.MarkVerifying(verification.Sas, verification.PeerFingerprint);
                });
                BeginVerification(item, session, verification);
                break;
            case TransferEvent.PeerVerified verified:
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
            case TransferEvent.Started:
                // A TCP connection was accepted. Wait for actual payload
                // progress before showing the transfer: the listener is on a
                // fixed port and scanners may connect with non-AirBridge data.
                break;
            case TransferEvent.Progress progress:
                if (!announced)
                {
                    announced = true;
                    Post(() =>
                    {
                        if (!Transfers.Contains(item))
                        {
                            Transfers.Insert(0, item);
                        }
                    });
                }

                Post(() =>
                {
                    if (!item.IsTerminal)
                    {
                        item.MarkRunning();
                        item.ApplyProgress(progress.BytesTransferred, progress.TotalBytes);
                    }
                });
                break;
            case TransferEvent.Completed completed when completed.Report.PairingOnly:
                announced = true;
                TryDeletePairingArtifacts(item.SourcePath);
                Post(() =>
                {
                    if (!Transfers.Contains(item))
                    {
                        Transfers.Insert(0, item);
                    }

                    item.MarkCompleted(completed.Report);
                });

                Notify("Device verified.", NoticeSeverity.Success);
                _ = DismissPairingCardAsync(item);
                break;
            case TransferEvent.Completed completed:
                announced = true;
                var finalPath = CommitReceivedFile(item.SourcePath, completed.Report, settings);
                Post(() =>
                {
                    if (!Transfers.Contains(item))
                    {
                        Transfers.Insert(0, item);
                    }

                    item.MarkCompleted(completed.Report);
                    if (finalPath is not null)
                    {
                        item.RenameAfterReceive(Path.GetFileName(finalPath), finalPath);
                    }
                });

                Notify($"Received {Path.GetFileName(finalPath) ?? item.FileName}.", NoticeSeverity.Success);
                break;
            case TransferEvent.Failed failed:
                if (!announced)
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
                    if (!Transfers.Contains(item))
                    {
                        Transfers.Insert(0, item);
                    }

                    if (!item.IsTerminal)
                    {
                        item.MarkFailed(string.IsNullOrWhiteSpace(failed.Message) ? failed.Kind : failed.Message);
                    }
                });
                break;
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

    /// <summary>Removes the unused staging file left by a pairing-only accept.</summary>
    private static void TryDeletePairingArtifacts(string path)
    {
        if (string.IsNullOrWhiteSpace(path))
        {
            return;
        }

        foreach (var candidate in new[] { path, path + ".part", path + ".part.state" })
        {
            try
            {
                if (File.Exists(candidate))
                {
                    File.Delete(candidate);
                }
            }
            catch (Exception error)
            {
                AppLog.Warn($"Could not remove the pairing staging file {candidate}.", error);
            }
        }
    }

    /// <summary>Renames a committed receive with a sniffed extension and a friendly name.</summary>
    private string? CommitReceivedFile(string committedPath, TransferReport report, AppSettings settings)
    {
        if (report.PairingOnly)
        {
            return null;
        }

        try
        {
            var path = string.IsNullOrWhiteSpace(report.DestinationPath) ? committedPath : report.DestinationPath;
            if (!File.Exists(path))
            {
                return null;
            }

            var directory = Path.GetDirectoryName(path);
            if (string.IsNullOrWhiteSpace(directory))
            {
                directory = settings.DownloadDirectory;
            }

            var extension = FileKinds.SniffExtension(path);
            var baseName = Path.GetFileNameWithoutExtension(
                FileKinds.BuildFriendlyName(report.PayloadId, DateTime.Now, string.Empty));
            var target = FileKinds.UniquePath(directory, baseName, extension);

            if (!string.Equals(path, target, StringComparison.OrdinalIgnoreCase))
            {
                File.Move(path, target);
            }

            return target;
        }
        catch (Exception error)
        {
            AppLog.Warn("The received file could not be renamed.", error);
            return null;
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
        FileTransferSession? session;

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
}
