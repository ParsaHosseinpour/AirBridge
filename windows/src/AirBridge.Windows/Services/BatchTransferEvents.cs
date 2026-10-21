using System;
using System.Threading;
using System.Threading.Tasks;
using AirBridge.Core;

namespace AirBridge.Windows.Services;

/// <summary>
/// Raises the pull-based <see cref="BatchTransferSession.NextEvent"/> stream as
/// .NET events, mirroring <see cref="FileTransferEvents"/> for batch sessions.
/// </summary>
/// <remarks>
/// <para>
/// Drive the session with <see cref="BatchTransferSession.Run"/> and collect
/// this pump at the same time: <c>Run</c> resolves with the final
/// <see cref="BatchReport"/>, while the pump surfaces manifest, file and
/// progress events as they happen.
/// </para>
/// <para>
/// The pump applies backpressure — the next event is requested only after the
/// current handler returns — so handlers must return quickly and must not
/// block. Events are raised on a thread-pool thread; the coordinator marshals
/// them onto the UI thread.
/// </para>
/// <para>
/// The stream is single-consumer and the session is one-shot. The flow
/// completes after the terminal event, and the session closes its stream at
/// the same moment; the pump then ends without further calls.
/// </para>
/// </remarks>
public sealed class BatchTransferEvents : IAsyncDisposable
{
    private readonly BatchTransferSession _session;
    private Task? _pump;

    /// <summary>Creates a pump for <paramref name="session"/>; call <see cref="Start"/> to begin reading.</summary>
    /// <exception cref="ArgumentNullException"><paramref name="session"/> is null.</exception>
    public BatchTransferEvents(BatchTransferSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
    }

    /// <summary>Raised for every event, in order.</summary>
    public event EventHandler<BatchEvent>? EventReceived;

    /// <summary>Raised once the stream is exhausted, whatever the outcome.</summary>
    public event EventHandler? Closed;

    /// <summary>Raised when reading the stream failed (the terminal event is then never raised).</summary>
    public event EventHandler<FfiException>? Faulted;

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

    /// <summary>Cancels the transfer and waits for the pump to end.</summary>
    public async ValueTask CancelAndDisposeAsync()
    {
        _session.Cancel();
        await Completion.ConfigureAwait(false);
    }

    /// <summary>
    /// Waits for the pump to end without touching the transfer. Use it after
    /// the terminal event (or after <see cref="BatchTransferSession.Run"/>
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

                if (next is BatchEvent.Finished or BatchEvent.Failed or BatchEvent.Cancelled)
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
