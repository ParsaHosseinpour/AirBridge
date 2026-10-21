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

    public TransferCoordinator(DispatcherQueue dispatcher, Func<AppSettings> settings)
    {
        _dispatcher = dispatcher ?? throw new ArgumentNullException(nameof(dispatcher));
        _settings = settings ?? throw new ArgumentNullException(nameof(settings));
    }

    /// <summary>All transfers, newest first. Only touched on the UI thread.</summary>
    public ObservableCollection<TransferItem> Transfers { get; } = new();

    /// <summary>True while an inbound listener is bound.</summary>
    public bool IsReceiverListening { get; private set; }

    public event EventHandler<TransferNotice>? Notice;
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

        while (!token.IsCancellationRequested)
        {
            Post(item.MarkConnecting);

            FileTransferSession? session = null;
            FileTransferEvents? events = null;

            try
            {
                session = FileTransferSession.Send(
                    item.Address,
                    item.SourcePath,
                    new SendOptions(
                        PayloadId: item.PayloadId,
                        ResponseTimeoutMs: 90_000,
                        ProgressIntervalMs: 100));
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
                events.EventReceived += (_, transferEvent) => OnTransferEvent(item, transferEvent);
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

    private void OnTransferEvent(TransferItem item, TransferEvent transferEvent)
    {
        switch (transferEvent)
        {
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

                session = FileTransferSession.Receive(
                    $"0.0.0.0:{settings.ListenPort}",
                    destination,
                    new ReceiveOptions(
                        ExpectedPayloadId: null,
                        AckEveryBytes: 4 * 1024 * 1024,
                        VerifyWholeFile: true,
                        AcceptTimeoutMs: AcceptTimeoutMs,
                        ProgressIntervalMs: 100));

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
                    OnReceiveEvent(receiveItem, transferEvent, settings, ref announced);
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
        TransferEvent transferEvent,
        AppSettings settings,
        ref bool announced)
    {
        switch (transferEvent)
        {
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

    /// <summary>Renames a committed receive with a sniffed extension and a friendly name.</summary>
    private string? CommitReceivedFile(string committedPath, TransferReport report, AppSettings settings)
    {
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
