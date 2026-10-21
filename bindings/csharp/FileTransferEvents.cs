using System;
using System.Threading;
using System.Threading.Tasks;

namespace AirBridge.Core;

/// <summary>
/// Raises the pull-based <see cref="FileTransferSession.NextEvent"/> stream as
/// .NET events: throttled progress samples plus one terminal event.
/// </summary>
/// <remarks>
/// <para>
/// Drive the session with <see cref="FileTransferSession.Run"/> and collect
/// this pump at the same time; <c>Run</c> resolves with the final
/// <see cref="TransferReport"/>, while the pump surfaces progress as it
/// happens. Both may be used together: progress numbers are read from the
/// stream adapter and never interfere with the transfer itself.
/// </para>
/// <para>
/// The pump applies backpressure — the next event is requested only after the
/// current handler returns — so handlers must return quickly and must not
/// block. Events are raised on a thread-pool thread.
/// </para>
/// <para>
/// The stream is single-consumer and the session is one-shot. The flow
/// completes after the terminal event, and the session closes its stream at
/// the same moment; the pump then ends without further calls.
/// </para>
/// </remarks>
public sealed class FileTransferEvents : IAsyncDisposable
{
    private readonly FileTransferSession _session;
    private Task? _pump;

    /// <summary>Creates a pump for <paramref name="session"/>; call <see cref="Start"/> to begin reading.</summary>
    /// <exception cref="ArgumentNullException"><paramref name="session"/> is null.</exception>
    public FileTransferEvents(FileTransferSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
    }

    /// <summary>Raised for every event, in order.</summary>
    public event EventHandler<TransferEvent>? EventReceived;

    /// <summary>Raised once the payload was verified and committed.</summary>
    public event EventHandler<TransferReport>? Completed;

    /// <summary>Raised when the transfer failed; carries the same classification as the thrown error.</summary>
    public event EventHandler<TransferEvent.Failed>? Failed;

    /// <summary>Raised when the transfer was stopped by <see cref="CancelAndDisposeAsync"/> or <see cref="FileTransferSession.Cancel"/>.</summary>
    public event EventHandler? Cancelled;

    /// <summary>Raised when reading the stream failed (the terminal event is then never raised).</summary>
    public event EventHandler<FfiException>? Faulted;

    /// <summary>
    /// Raised once the stream is exhausted, whatever the outcome. Use it to
    /// release UI resources without distinguishing Completed/Failed/Cancelled.
    /// </summary>
    public event EventHandler? Closed;

    /// <summary>Starts the pump. Idempotent: extra calls are ignored.</summary>
    public void Start()
    {
        Interlocked.CompareExchange(ref _pump, Task.Run(PumpAsync), null);
    }

    /// <summary>
    /// Completes when the pump task has finished. Before <see cref="Start"/>
    /// this is already complete.
    /// </summary>
    public Task Completion => Volatile.Read(ref _pump) ?? Task.CompletedTask;

    /// <summary>
    /// Cancels the transfer and waits for the pump to end.
    /// </summary>
    /// <remarks>
    /// The pump cannot be detached from the stream: it is a pending read that
    /// only resolves when the transfer reaches a terminal state (cancelled,
    /// failed, or completed). This method forces that state first, so it
    /// returns promptly even while the transfer is still moving bytes. The
    /// receiver's part file and sidecar survive, so retrying with the same
    /// payload id resumes rather than restarts.
    /// </remarks>
    public async ValueTask CancelAndDisposeAsync()
    {
        _session.Cancel();
        await Completion.ConfigureAwait(false);
    }

    /// <summary>
    /// Waits for the pump to end without touching the transfer. Use it after
    /// the terminal event (or after <see cref="FileTransferSession.Run"/>
    /// returned); if the transfer is still running this blocks until it
    /// finishes. Prefer <see cref="CancelAndDisposeAsync"/> to stop early.
    /// </summary>
    public ValueTask DisposeAsync() => new(Completion);

    private async Task PumpAsync()
    {
        try
        {
            while (true)
            {
                var next = await _session.NextEvent().ConfigureAwait(false);
                if (next is null)
                {
                    break;
                }

                EventReceived?.Invoke(this, next);

                switch (next)
                {
                    case TransferEvent.Completed completed:
                        Completed?.Invoke(this, completed.Report);
                        break;
                    case TransferEvent.Failed failed:
                        Failed?.Invoke(this, failed);
                        break;
                    case TransferEvent.Cancelled:
                        Cancelled?.Invoke(this, EventArgs.Empty);
                        break;
                }

                if (next is TransferEvent.Completed or TransferEvent.Failed or TransferEvent.Cancelled)
                {
                    break;
                }
            }

            Closed?.Invoke(this, EventArgs.Empty);
        }
        catch (FfiException error)
        {
            Faulted?.Invoke(this, error);
        }
    }
}
