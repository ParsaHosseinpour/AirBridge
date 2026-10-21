using System;
using System.Threading;
using System.Threading.Tasks;

namespace AirBridge.Core;

/// <summary>
/// Raises the pull-based <see cref="PeerDiscoveryEngine.NextEvent"/> stream as
/// .NET events, pumped from a background task.
/// </summary>
/// <remarks>
/// <para>
/// The Rust engine exposes discovery as a pull-based async sequence, so the
/// foreign side can never be called from a Tokio worker. That is the right
/// boundary design but it is awkward in C#; this class supplies the
/// event-shaped API without giving up the pull: one background task awaits
/// <see cref="PeerDiscoveryEngine.NextEvent"/> in a loop and raises
/// <see cref="EventReceived"/> for every event, in order.
/// </para>
/// <para>
/// The pump applies backpressure. The next event is not requested until the
/// current handler has returned, so a slow handler throttles the pump instead
/// of building an unbounded queue. Handlers run on a thread-pool thread and
/// must not block for long; dispatch to the UI thread (or a channel) inside
/// the handler instead.
/// </para>
/// <para>
/// The underlying stream is single-consumer: do not call
/// <see cref="PeerDiscoveryEngine.NextEvent"/> yourself while the pump is
/// running, and keep one pump per engine. When the consumer falls behind, the
/// engine reports <see cref="DiscoveryEvent.Lagged"/> rather than growing
/// without bound; <see cref="PeerDiscoveryEngine.Peers"/> is the
/// authoritative snapshot.
/// </para>
/// </remarks>
public sealed class PeerDiscoveryEvents : IAsyncDisposable
{
    private readonly PeerDiscoveryEngine _engine;
    private Task? _pump;

    /// <summary>Creates a pump for <paramref name="engine"/>; call <see cref="Start"/> to begin reading.</summary>
    /// <exception cref="ArgumentNullException"><paramref name="engine"/> is null.</exception>
    public PeerDiscoveryEvents(PeerDiscoveryEngine engine)
    {
        _engine = engine ?? throw new ArgumentNullException(nameof(engine));
    }

    /// <summary>Raised for every discovery event, in order.</summary>
    public event EventHandler<DiscoveryEvent>? EventReceived;

    /// <summary>
    /// Raised once the stream has ended (the engine was stopped, or the
    /// coordinator shut down on its own).
    /// </summary>
    public event EventHandler? Closed;

    /// <summary>
    /// Raised when reading the stream failed. The pump then stops; the
    /// stream remains closed.
    /// </summary>
    public event EventHandler<FfiException>? Faulted;

    /// <summary>Starts the pump. Idempotent: extra calls are ignored.</summary>
    public void Start()
    {
        Interlocked.CompareExchange(ref _pump, Task.Run(PumpAsync), null);
    }

    /// <summary>
    /// Completes when the pump task has finished, successfully or not. Before
    /// <see cref="Start"/> this is already complete.
    /// </summary>
    public Task Completion => Volatile.Read(ref _pump) ?? Task.CompletedTask;

    /// <summary>
    /// Stops the engine (non-blocking and idempotent) and waits for the pump
    /// to end.
    /// </summary>
    /// <remarks>
    /// There is deliberately no "detach without stopping": the pump is a
    /// pending read on the engine's stream, and the only way to resolve it is
    /// to end the stream. <see cref="PeerDiscoveryEngine.Stop"/> publishes
    /// <see cref="DiscoveryEvent.Shutdown"/>, closes the stream, and the pump
    /// observes <see langword="null"/> and finishes. Wait without stopping via
    /// <see cref="Completion"/> if the engine must keep running.
    /// </remarks>
    public async ValueTask DisposeAsync()
    {
        _engine.Stop();
        await Completion.ConfigureAwait(false);
    }

    private async Task PumpAsync()
    {
        try
        {
            while (true)
            {
                var next = await _engine.NextEvent().ConfigureAwait(false);
                if (next is null)
                {
                    break;
                }

                EventReceived?.Invoke(this, next);

                if (next is DiscoveryEvent.Shutdown)
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
