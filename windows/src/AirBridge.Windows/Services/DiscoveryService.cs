using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using AirBridge.Core;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Services;

public sealed class DiscoveryPeersEventArgs : EventArgs
{
    public DiscoveryPeersEventArgs(
        IReadOnlyList<PeerInfo> peers,
        string? peerIdHex,
        bool running,
        bool starting,
        string? error)
    {
        Peers = peers;
        PeerIdHex = peerIdHex;
        Running = running;
        Starting = starting;
        Error = error;
    }

    public IReadOnlyList<PeerInfo> Peers { get; }
    public string? PeerIdHex { get; }
    public bool Running { get; }
    public bool Starting { get; }
    public string? Error { get; }
}

public sealed class DiscoveryTierEventArgs : EventArgs
{
    public DiscoveryTierEventArgs(DiscoveryTier tier, TierState state)
    {
        Tier = tier;
        State = state;
    }

    public DiscoveryTier Tier { get; }
    public TierState State { get; }
}

public sealed class DiscoveryTierErrorEventArgs : EventArgs
{
    public DiscoveryTierErrorEventArgs(DiscoveryTier tier, string message)
    {
        Tier = tier;
        Message = message;
    }

    public DiscoveryTier Tier { get; }
    public string Message { get; }
}

/// <summary>
/// Owns the native <see cref="PeerDiscoveryEngine"/> and turns its pull-based
/// event stream into ordinary .NET events.
///
/// <see cref="PeerDiscoveryEvents"/> applies backpressure by awaiting the next
/// event only after the handler returns. The handlers here therefore only
/// merge a small peer table and raise an immutable snapshot; the UI marshals
/// that snapshot to the dispatcher.
/// </summary>
public sealed class DiscoveryService : IAsyncDisposable
{
    private readonly SemaphoreSlim _gate = new(1, 1);
    private readonly object _sync = new();
    private readonly Dictionary<string, PeerInfo> _peers = new(StringComparer.OrdinalIgnoreCase);

    private PeerDiscoveryEngine? _engine;
    private PeerDiscoveryEvents? _events;
    private WindowsBleBackend? _bleBackend;
    private CancellationTokenSource? _refreshCts;
    private Task? _refreshTask;

    private bool _running;
    private bool _starting;
    private string? _error;
    private string? _peerIdHex;

    public event EventHandler<DiscoveryPeersEventArgs>? PeersChanged;
    public event EventHandler<DiscoveryTierEventArgs>? TierChanged;
    public event EventHandler<DiscoveryTierErrorEventArgs>? TierError;
    public event EventHandler<string>? PeerIdAvailable;
    public event EventHandler<string>? ScanActivity;

    public bool IsRunning
    {
        get
        {
            lock (_sync)
            {
                return _running;
            }
        }
    }

    public string? PeerIdHex
    {
        get
        {
            lock (_sync)
            {
                return _peerIdHex;
            }
        }
    }

    /// <summary>
    /// (Re)starts discovery. Runs the synchronous native startup on the calling
    /// thread, so callers should invoke it from a background task.
    /// </summary>
    public async Task StartAsync(AppSettings settings)
    {
        ArgumentNullException.ThrowIfNull(settings);

        await _gate.WaitAsync().ConfigureAwait(false);
        try
        {
            await StopLockedAsync().ConfigureAwait(false);

            lock (_sync)
            {
                _starting = true;
                _error = null;
                _peers.Clear();
                _peerIdHex = settings.PeerIdHex;
            }

            PublishPeers();

            var native = NativeCore.Initialize();
            if (!native.Available)
            {
                lock (_sync)
                {
                    _starting = false;
                    _running = false;
                    _error = native.Error ?? "The native AirBridge core is unavailable.";
                }

                PublishPeers();
                return;
            }

            PeerDiscoveryEngine? engine = null;
            PeerDiscoveryEvents? events = null;
            WindowsBleBackend? backend = null;

            try
            {
                var config = new DiscoveryConfig(
                    PeerIdHex: string.IsNullOrWhiteSpace(settings.PeerIdHex) ? null : settings.PeerIdHex,
                    PeerName: settings.DisplayName,
                    ListenPort: settings.ListenPort,
                    EnableBle: settings.EnableBle,
                    EnableMdns: settings.EnableMdns,
                    SubnetPolicy: settings.SubnetPolicy,
                    MdnsGraceMs: 15_000,
                    PeerTtlMs: 60_000);

                engine = new PeerDiscoveryEngine(config);
                events = new PeerDiscoveryEvents(engine);
                events.EventReceived += OnDiscoveryEvent;
                events.Closed += OnEventsClosed;
                events.Faulted += OnEventsFaulted;

                if (settings.EnableBle)
                {
                    backend = new WindowsBleBackend();
                }

                // Order matters: NextEvent() returns null while the engine is
                // stopped, so the pump may only start after Start().
                engine.Start(backend);
                events.Start();

                var peerId = engine.PeerIdHex();
                if (!string.IsNullOrWhiteSpace(peerId))
                {
                    lock (_sync)
                    {
                        _peerIdHex = peerId;
                    }

                    PeerIdAvailable?.Invoke(this, peerId);
                }

                lock (_sync)
                {
                    _engine = engine;
                    _events = events;
                    _bleBackend = backend;
                    _running = true;
                    _starting = false;
                    _error = null;
                }

                AppLog.Info($"Discovery started as \"{settings.DisplayName}\" on port {settings.ListenPort}.");
                StartRefreshLoop(engine);
                RefreshPeers();
                PublishPeers();
            }
            catch (Exception error)
            {
                AppLog.Error("Discovery failed to start.", error);

                if (events is not null)
                {
                    events.EventReceived -= OnDiscoveryEvent;
                    events.Closed -= OnEventsClosed;
                    events.Faulted -= OnEventsFaulted;
                    try
                    {
                        await events.DisposeAsync().ConfigureAwait(false);
                    }
                    catch (Exception stopError)
                    {
                        AppLog.Warn("Could not stop the failed discovery pump.", stopError);
                    }
                }
                else
                {
                    try
                    {
                        engine?.Stop();
                    }
                    catch
                    {
                        // The engine may never have started.
                    }
                }

                engine?.Dispose();
                backend?.Dispose();

                lock (_sync)
                {
                    _running = false;
                    _starting = false;
                    _error = Format.FriendlyException(error);
                }

                PublishPeers();
            }
        }
        finally
        {
            _gate.Release();
        }
    }

    public async Task StopAsync()
    {
        await _gate.WaitAsync().ConfigureAwait(false);
        try
        {
            await StopLockedAsync().ConfigureAwait(false);
            PublishPeers();
        }
        finally
        {
            _gate.Release();
        }
    }

    public void TriggerSubnetScan()
    {
        try
        {
            var engine = GetEngine();
            engine?.TriggerSubnetScan();
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not trigger a subnet scan.", error);
        }
    }

    private async Task StopLockedAsync()
    {
        var refreshCts = _refreshCts;
        _refreshCts = null;
        var refreshTask = _refreshTask;
        _refreshTask = null;

        if (refreshCts is not null)
        {
            try
            {
                refreshCts.Cancel();
            }
            catch
            {
                // Already disposed.
            }
        }

        if (refreshTask is not null)
        {
            try
            {
                await refreshTask.ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                // Expected.
            }
            catch (Exception error)
            {
                AppLog.Warn("The discovery refresh loop ended unexpectedly.", error);
            }
        }

        refreshCts?.Dispose();

        PeerDiscoveryEvents? events;
        PeerDiscoveryEngine? engine;
        WindowsBleBackend? backend;

        lock (_sync)
        {
            events = _events;
            engine = _engine;
            backend = _bleBackend;
            _events = null;
            _engine = null;
            _bleBackend = null;
            _running = false;
            _starting = false;
            _peers.Clear();
        }

        if (events is not null)
        {
            events.EventReceived -= OnDiscoveryEvent;
            events.Closed -= OnEventsClosed;
            events.Faulted -= OnEventsFaulted;

            try
            {
                // DisposeAsync calls engine.Stop() and waits for the pending
                // NextEvent() read to resolve.
                await events.DisposeAsync().ConfigureAwait(false);
            }
            catch (Exception error)
            {
                AppLog.Warn("The discovery pump did not close cleanly.", error);
            }
        }
        else
        {
            try
            {
                engine?.Stop();
            }
            catch
            {
                // Not started.
            }
        }

        engine?.Dispose();
        backend?.Dispose();
    }

    private PeerDiscoveryEngine? GetEngine()
    {
        lock (_sync)
        {
            return _engine;
        }
    }

    private void StartRefreshLoop(PeerDiscoveryEngine engine)
    {
        var cts = new CancellationTokenSource();
        _refreshCts = cts;
        _refreshTask = Task.Run(async () =>
        {
            using var timer = new PeriodicTimer(TimeSpan.FromSeconds(2));
            try
            {
                while (await timer.WaitForNextTickAsync(cts.Token).ConfigureAwait(false))
                {
                    if (!ReferenceEquals(GetEngine(), engine))
                    {
                        break;
                    }

                    RefreshPeers();
                }
            }
            catch (OperationCanceledException)
            {
                // Normal shutdown.
            }
        });
    }

    private void RefreshPeers()
    {
        var engine = GetEngine();
        if (engine is null)
        {
            return;
        }

        try
        {
            var snapshot = engine.Peers();
            lock (_sync)
            {
                _peers.Clear();
                foreach (var peer in snapshot)
                {
                    _peers[peer.IdHex] = peer;
                }
            }

            PublishPeers();
        }
        catch (FfiException.NotStarted)
        {
            // Discovery is between restarts.
        }
        catch (ObjectDisposedException)
        {
            // Racing with shutdown.
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not read the peer snapshot.", error);
        }
    }

    private void OnDiscoveryEvent(object? sender, DiscoveryEvent discoveryEvent)
    {
        switch (discoveryEvent)
        {
            case DiscoveryEvent.PeerFound found:
                UpsertPeer(found.Peer);
                break;
            case DiscoveryEvent.PeerUpdated updated:
                UpsertPeer(updated.Peer);
                break;
            case DiscoveryEvent.PeerLost lost:
                lock (_sync)
                {
                    _peers.Remove(lost.IdHex);
                }

                PublishPeers();
                break;
            case DiscoveryEvent.TierStateChanged tier:
                TierChanged?.Invoke(this, new DiscoveryTierEventArgs(tier.Tier, tier.State));
                break;
            case DiscoveryEvent.TierError error:
                AppLog.WarnEvery(
                    $"tier:{error.Tier}:{error.Message}",
                    TimeSpan.FromMinutes(1),
                    $"Discovery tier {error.Tier} reported: {error.Message}");
                TierError?.Invoke(this, new DiscoveryTierErrorEventArgs(error.Tier, error.Message));
                break;
            case DiscoveryEvent.SubnetScanStarted started:
                ScanActivity?.Invoke(this, $"Subnet sweep started across {started.Subnets} network(s).");
                break;
            case DiscoveryEvent.SubnetScanFinished finished:
                ScanActivity?.Invoke(
                    this,
                    $"Subnet sweep finished: {finished.Responded} of {finished.Probed} hosts replied, {finished.Discovered} new peer(s).");
                break;
            case DiscoveryEvent.Lagged:
                RefreshPeers();
                break;
            case DiscoveryEvent.Shutdown:
                lock (_sync)
                {
                    _running = false;
                }

                PublishPeers();
                break;
        }
    }

    private void UpsertPeer(PeerInfo peer)
    {
        lock (_sync)
        {
            _peers[peer.IdHex] = peer;
        }

        PublishPeers();
    }

    private void OnEventsClosed(object? sender, EventArgs args)
    {
        lock (_sync)
        {
            _running = false;
        }

        PublishPeers();
    }

    private void OnEventsFaulted(object? sender, FfiException error)
    {
        AppLog.Error("The discovery event stream failed.", error);
        lock (_sync)
        {
            _running = false;
            _error = Format.FriendlyException(error);
        }

        PublishPeers();
    }

    private void PublishPeers()
    {
        DiscoveryPeersEventArgs args;
        lock (_sync)
        {
            var peers = _peers.Values
                .OrderBy(peer => peer.Name ?? peer.IdHex, StringComparer.CurrentCultureIgnoreCase)
                .ToArray();
            args = new DiscoveryPeersEventArgs(peers, _peerIdHex, _running, _starting, _error);
        }

        PeersChanged?.Invoke(this, args);
    }

    public async ValueTask DisposeAsync()
    {
        await StopAsync().ConfigureAwait(false);
        _gate.Dispose();
    }
}
