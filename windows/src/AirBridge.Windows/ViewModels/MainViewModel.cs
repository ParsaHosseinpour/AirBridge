using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Collections.Specialized;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Threading.Tasks;
using AirBridge.Core;
using AirBridge.Windows.Core;
using AirBridge.Windows.Models;
using AirBridge.Windows.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage;
using TransferDirection = AirBridge.Windows.Models.TransferDirection;

namespace AirBridge.Windows.ViewModels;

/// <summary>
/// The single view model behind the main window. It owns settings, the
/// discovery service and the transfer coordinator, and it is the only place
/// that maps native state to observable properties.
///
/// Every mutation of an observable property happens on the UI thread: the
/// discovery and transfer services raise events on thread-pool threads, and
/// this class marshals them through the dispatcher.
/// </summary>
public sealed class MainViewModel : ObservableObject, IAsyncDisposable
{
    private const ulong PairingTimeoutMs = 30_000;

    private readonly DispatcherQueue _dispatcher;
    private readonly SettingsStore _store;
    private readonly DiscoveryService _discovery = new();
    private readonly TransferCoordinator _coordinator;
    private readonly FirewallService _firewall = new();
    private readonly Dictionary<string, PeerDevice> _peerIndex = new(StringComparer.OrdinalIgnoreCase);
    private readonly HashSet<TransferItem> _observedTransfers = new();

    private AppSettings _settings;
    private PeerIdentity? _identity;
    private byte[]? _identityPkcs8;
    private bool _initialized;
    private bool _disposed;

    private string _displayName = "My PC";
    private string _statusText = "Starting";
    private string _statusColor = "#FF9FE7FF";
    private string _statusDetail = "Waking up the radar\u2026";
    private bool _isScanning;
    private string _peerCountText = "None nearby";
    private string _scanNote = "Discovery uses Bluetooth, mDNS and a local subnet sweep.";
    private string _transferHeaderText = "Transfers";
    private bool _hasTransfers;
    private PeerDevice? _selectedPeer;
    private bool _hasStagedFiles;
    private string _stagedSummaryText = "No files staged";
    private string _receiverStatusText = "Starting\u2026";
    private string _receiverStatusColor = "#FF9FE7FF";
    private string _firewallStatusText = "Checking firewall\u2026";
    private string _firewallStatusColor = "#FF9DB2C4";
    private bool _firewallIsReady;
    private bool _isBannerOpen;
    private string _bannerText = string.Empty;
    private InfoBarSeverity _bannerSeverity = InfoBarSeverity.Informational;
    private bool _isBusy;
    private string _nativeStatusText = "Loading the native core\u2026";
    private bool _nativeReady;
    private bool _hasPeers;

    public MainViewModel(DispatcherQueue dispatcher, SettingsStore store)
    {
        _dispatcher = dispatcher ?? throw new ArgumentNullException(nameof(dispatcher));
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _settings = _store.Load();
        _coordinator = new TransferCoordinator(
            dispatcher,
            () => _settings,
            () => _identityPkcs8,
            peerId => _peerIndex.TryGetValue(peerId, out var peer) ? peer.Name : null);

        _displayName = _settings.DisplayName;
        _receiverStatusColor = _settings.ReceiveEnabled ? "#FF9FE7FF" : "#FF9DB2C4";
        _receiverStatusText = _settings.ReceiveEnabled ? "Starting\u2026" : "Off";

        Tiers = new ObservableCollection<TierInfo>
        {
            new(DiscoveryTier.Ble, "Bluetooth", "\uE702"),
            new(DiscoveryTier.Mdns, "Wi-Fi / mDNS", "\uE701"),
            new(DiscoveryTier.Subnet, "Subnet sweep", "\uE968"),
        };

        foreach (var tier in Tiers)
        {
            tier.Reset();
        }

        Attach();
    }

    /// <summary>Set by the window: shows the real SAS dialog and returns the user's answer.</summary>
    public Func<SasPrompt, Task<bool>>? VerificationPrompt { get; set; }

    /// <summary>Set by the window: opens the file picker and returns the chosen files.</summary>
    public Func<Task<IReadOnlyList<StorageFile>>>? FilePicker { get; set; }

    public event EventHandler? ScrollTransfersIntoView;

    public ObservableCollection<PeerDevice> Peers { get; } = new();

    public ObservableCollection<StagedFile> StagedFiles { get; } = new();

    public ObservableCollection<TransferItem> Transfers => _coordinator.Transfers;

    public ObservableCollection<TierInfo> Tiers { get; }

    public AppSettings Settings => _settings;

    public string AppVersion { get; } =
        Assembly.GetExecutingAssembly().GetName().Version?.ToString(3) ?? "1.0.0";

    public string DisplayName
    {
        get => _displayName;
        private set
        {
            if (SetProperty(ref _displayName, value))
            {
                OnPropertiesChanged(nameof(Initials));
            }
        }
    }

    public string Initials => Format.Initials(DisplayName);

    public string PeerIdText =>
        string.IsNullOrWhiteSpace(_settings.PeerIdHex) ? "Not assigned yet" : _settings.PeerIdHex;

    public string NativeStatusText
    {
        get => _nativeStatusText;
        private set => SetProperty(ref _nativeStatusText, value);
    }

    public bool NativeReady
    {
        get => _nativeReady;
        private set
        {
            if (SetProperty(ref _nativeReady, value))
            {
                OnPropertiesChanged(nameof(CanSend), nameof(CanVerifySelectedPeer));
            }
        }
    }

    public string StatusText
    {
        get => _statusText;
        private set => SetProperty(ref _statusText, value);
    }

    public string StatusColor
    {
        get => _statusColor;
        private set => SetProperty(ref _statusColor, value);
    }

    public string StatusDetail
    {
        get => _statusDetail;
        private set => SetProperty(ref _statusDetail, value);
    }

    public bool IsScanning
    {
        get => _isScanning;
        private set => SetProperty(ref _isScanning, value);
    }

    public string PeerCountText
    {
        get => _peerCountText;
        private set => SetProperty(ref _peerCountText, value);
    }

    public bool HasPeers
    {
        get => _hasPeers;
        private set => SetProperty(ref _hasPeers, value);
    }

    public string ScanNote
    {
        get => _scanNote;
        private set => SetProperty(ref _scanNote, value);
    }

    public string TransferHeaderText
    {
        get => _transferHeaderText;
        private set => SetProperty(ref _transferHeaderText, value);
    }

    public bool HasTransfers
    {
        get => _hasTransfers;
        private set => SetProperty(ref _hasTransfers, value);
    }

    public PeerDevice? SelectedPeer
    {
        get => _selectedPeer;
        private set
        {
            if (!SetProperty(ref _selectedPeer, value))
            {
                return;
            }

            OnPropertiesChanged(
                nameof(HasSelectedPeer),
                nameof(SelectedPeerName),
                nameof(SelectedPeerDetail),
                nameof(SendButtonText),
                nameof(CanSend),
                nameof(CanVerifySelectedPeer));
        }
    }

    public bool HasSelectedPeer => SelectedPeer is not null;

    public string SelectedPeerName => SelectedPeer?.Name ?? "No device selected";

    public string SelectedPeerDetail => SelectedPeer switch
    {
        null => "Tap a device in the radar or the list.",
        { HasAddress: false } peer => $"{peer.DeviceLabel} \u2014 waiting for an IP address",
        { IsTrusted: true } peer => $"{peer.DeviceLabel} \u2014 verified \u2014 {peer.PrimaryAddress}",
        { } peer => $"{peer.DeviceLabel} \u2014 not verified \u2014 {peer.PrimaryAddress}",
    };

    public string SendButtonText
    {
        get
        {
            if (SelectedPeer is null)
            {
                return "Select a device";
            }

            if (StagedFiles.Count == 0)
            {
                return "Choose files first";
            }

            var verb = SelectedPeer.IsTrusted ? "Send" : "Verify & send";
            var count = StagedFiles.Count == 1 ? StagedFiles[0].Name : $"{StagedFiles.Count} files";
            return $"{verb} {count}";
        }
    }

    public bool CanSend => SelectedPeer is not null && StagedFiles.Count > 0 && !IsBusy && _nativeReady;

    /// <summary>
    /// True when the selected device is already trusted but the user wants to
    /// compare its code again, e.g. after a suspected key change or reinstall.
    /// </summary>
    public bool CanVerifySelectedPeer =>
        SelectedPeer is { HasAddress: true, IsTrusted: true } && !IsBusy && _nativeReady;

    public bool HasStagedFiles
    {
        get => _hasStagedFiles;
        private set => SetProperty(ref _hasStagedFiles, value);
    }

    public string StagedSummaryText
    {
        get => _stagedSummaryText;
        private set => SetProperty(ref _stagedSummaryText, value);
    }

    public string ListenPortText => $"Port {_settings.ListenPort}";

    public string DownloadDirectory => _settings.DownloadDirectory;

    public bool ReceiverEnabled
    {
        get => _settings.ReceiveEnabled;
        set
        {
            if (_settings.ReceiveEnabled == value)
            {
                return;
            }

            _settings.ReceiveEnabled = value;
            _store.Save(_settings);
            OnPropertiesChanged(nameof(ReceiverEnabled));
            UpdateReceiverStatus();
            _ = _coordinator.RestartReceiverAsync();
            ShowBanner(
                value ? "Incoming transfers are enabled." : "Incoming transfers are paused.",
                value ? NoticeSeverity.Success : NoticeSeverity.Informational);
        }
    }

    public string ReceiverStatusText
    {
        get => _receiverStatusText;
        private set => SetProperty(ref _receiverStatusText, value);
    }

    public string ReceiverStatusColor
    {
        get => _receiverStatusColor;
        private set => SetProperty(ref _receiverStatusColor, value);
    }

    public string FirewallStatusText
    {
        get => _firewallStatusText;
        private set => SetProperty(ref _firewallStatusText, value);
    }

    public string FirewallStatusColor
    {
        get => _firewallStatusColor;
        private set => SetProperty(ref _firewallStatusColor, value);
    }

    public bool FirewallIsReady
    {
        get => _firewallIsReady;
        private set => SetProperty(ref _firewallIsReady, value);
    }

    public bool IsBannerOpen
    {
        get => _isBannerOpen;
        set => SetProperty(ref _isBannerOpen, value);
    }

    public string BannerText
    {
        get => _bannerText;
        private set => SetProperty(ref _bannerText, value);
    }

    public InfoBarSeverity BannerSeverity
    {
        get => _bannerSeverity;
        private set => SetProperty(ref _bannerSeverity, value);
    }

    public bool IsBusy
    {
        get => _isBusy;
        private set
        {
            if (SetProperty(ref _isBusy, value))
            {
                OnPropertiesChanged(nameof(CanSend), nameof(CanVerifySelectedPeer));
            }
        }
    }

    // ------------------------------------------------------------ lifecycle

    public async Task InitializeAsync()
    {
        if (_initialized || _disposed)
        {
            return;
        }

        _initialized = true;

        var native = NativeCore.Initialize();
        NativeReady = native.Available;
        NativeStatusText = native.Summary;

        if (!native.Available)
        {
            ShowBanner(
                native.Error ?? "The native AirBridge core is unavailable.",
                NoticeSeverity.Error);

            StatusText = "Native core unavailable";
            StatusColor = "#FFFF6B7A";
            StatusDetail = native.Error ?? "The native AirBridge core is unavailable.";
            IsScanning = false;
            await RefreshFirewallStatusAsync().ConfigureAwait(true);
            return;
        }

        if (!TryEnsureIdentity())
        {
            NativeReady = false;
            NativeStatusText = "Native core ready, but the device identity could not be created.";
            StatusText = "Identity unavailable";
            StatusColor = "#FFFF6B7A";
            StatusDetail = "AirBridge could not create the long-lived identity key required for secure pairing.";
            IsScanning = false;
            await RefreshFirewallStatusAsync().ConfigureAwait(true);
            return;
        }

        _coordinator.Start(_settings);

        if (_settings.AutoScanOnLaunch)
        {
            await Task.Run(() => _discovery.StartAsync(_settings)).ConfigureAwait(true);
        }
        else
        {
            SetIdleState();
        }

        await RefreshFirewallStatusAsync().ConfigureAwait(true);
    }

    public async Task RefreshAsync()
    {
        if (!NativeReady)
        {
            ShowBanner(NativeStatusText, NoticeSeverity.Error);
            return;
        }

        StatusText = "Restarting";
        StatusDetail = "Rescanning for nearby devices\u2026";

        foreach (var tier in Tiers)
        {
            tier.Reset();
        }

        await Task.Run(() => _discovery.StartAsync(_settings)).ConfigureAwait(true);
    }

    public async Task ApplySettingsAsync(AppSettings updated)
    {
        ArgumentNullException.ThrowIfNull(updated);

        var restartDiscovery =
            !string.Equals(updated.DisplayName, _settings.DisplayName, StringComparison.Ordinal) ||
            updated.ListenPort != _settings.ListenPort ||
            updated.EnableBle != _settings.EnableBle ||
            updated.EnableMdns != _settings.EnableMdns ||
            updated.SubnetPolicy != _settings.SubnetPolicy;

        var restartReceiver =
            updated.ListenPort != _settings.ListenPort ||
            updated.ReceiveEnabled != _settings.ReceiveEnabled ||
            !string.Equals(updated.DownloadDirectory, _settings.DownloadDirectory, StringComparison.OrdinalIgnoreCase);

        _settings = updated;
        _store.Save(updated);

        DisplayName = updated.DisplayName;
        OnPropertiesChanged(
            nameof(PeerIdText),
            nameof(ListenPortText),
            nameof(DownloadDirectory),
            nameof(ReceiverEnabled));

        foreach (var peer in Peers)
        {
            peer.IsTrusted = updated.IsTrusted(peer.IdHex);
        }

        UpdateReceiverStatus();
        OnPropertiesChanged(nameof(CanSend), nameof(SendButtonText));

        if (restartDiscovery)
        {
            await RefreshAsync();
        }
        else if (updated.AutoScanOnLaunch && !_discovery.IsRunning && NativeReady)
        {
            // Auto-scan was switched back on while discovery sat idle.
            await RefreshAsync();
        }

        if (restartReceiver)
        {
            await _coordinator.RestartReceiverAsync();
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        await _coordinator.DisposeAsync().ConfigureAwait(false);
        await _discovery.DisposeAsync().ConfigureAwait(false);

        _identityPkcs8 = null;
        _identity?.Dispose();
        _identity = null;
    }

    /// <summary>
    /// Loads the persistent P-256 identity once the native core is loaded, or
    /// creates it on first launch. The PKCS#8 bytes never leave the machine.
    /// </summary>
    private bool TryEnsureIdentity()
    {
        if (_identity is not null && _identityPkcs8 is { Length: > 0 })
        {
            return true;
        }

        try
        {
            var stored = _store.LoadIdentity();
            if (stored is { Length: > 0 })
            {
                try
                {
                    _identity = PeerIdentity.FromPkcs8(stored);
                }
                catch (Exception error)
                {
                    AppLog.Warn("The stored identity is unusable; generating a new one.", error);
                }
            }

            if (_identity is null)
            {
                _identity = PeerIdentity.Generate();
                _store.SaveIdentity(_identity.ToPkcs8());
            }

            _identityPkcs8 = _identity.ToPkcs8();
            return true;
        }
        catch (Exception error)
        {
            AppLog.Error("Could not create the device identity.", error);
            ShowBanner(
                $"The secure device identity could not be created: {Format.FriendlyException(error)}",
                NoticeSeverity.Error);
            return false;
        }
    }

    private void Attach()
    {
        _discovery.PeersChanged += (_, args) => Dispatch(() => ApplyPeers(args));
        _discovery.TierChanged += (_, args) => Dispatch(() => ApplyTier(args));
        _discovery.TierError += (_, args) => Dispatch(() => ApplyTierError(args));
        _discovery.PeerIdAvailable += (_, peerId) => Dispatch(() => ApplyPeerId(peerId));
        _discovery.ScanActivity += (_, message) => Dispatch(() => ScanNote = message);

        _coordinator.Notice += (_, notice) => Dispatch(() => ShowBanner(notice.Message, notice.Severity));
        _coordinator.ReceiverStateChanged += (_, _) => Dispatch(UpdateReceiverStatus);
        _coordinator.VerificationPrompt = PromptForVerificationAsync;
        _coordinator.PeerVerified += (_, args) => Dispatch(() => OnPeerVerified(args));

        Transfers.CollectionChanged += OnTransfersChanged;
        StagedFiles.CollectionChanged += OnStagedFilesChanged;
    }

    private void OnTransfersChanged(object? sender, NotifyCollectionChangedEventArgs args)
    {
        if (args.Action == NotifyCollectionChangedAction.Reset)
        {
            foreach (var item in _observedTransfers)
            {
                item.PropertyChanged -= OnTransferPropertyChanged;
            }

            _observedTransfers.Clear();
            foreach (var item in Transfers)
            {
                ObserveTransfer(item);
            }
        }
        else
        {
            if (args.OldItems is not null)
            {
                foreach (TransferItem item in args.OldItems)
                {
                    item.PropertyChanged -= OnTransferPropertyChanged;
                    _observedTransfers.Remove(item);
                }
            }

            if (args.NewItems is not null)
            {
                foreach (TransferItem item in args.NewItems)
                {
                    ObserveTransfer(item);
                }
            }
        }

        HasTransfers = Transfers.Count > 0;
        TransferHeaderText = Transfers.Count switch
        {
            0 => "Transfers",
            1 => "1 transfer",
            _ => $"{Transfers.Count} transfers",
        };
    }

    private void ObserveTransfer(TransferItem item)
    {
        if (_observedTransfers.Add(item))
        {
            item.PropertyChanged += OnTransferPropertyChanged;
        }
    }

    /// <summary>
    /// Keeps the info bar in step with the transfer cards. Cancelling and
    /// pausing are asynchronous, so the banner follows the terminal phase
    /// instead of claiming the transfer already stopped.
    /// </summary>
    private void OnTransferPropertyChanged(object? sender, PropertyChangedEventArgs args)
    {
        if (args.PropertyName != nameof(TransferItem.Phase) || sender is not TransferItem item)
        {
            return;
        }

        switch (item.Phase)
        {
            case TransferPhase.Running:
                ShowBanner(
                    item.Direction == TransferDirection.Send
                        ? $"Sending {item.FileName} to {item.PeerName}."
                        : $"Receiving {item.FileName} from {item.PeerName}.",
                    NoticeSeverity.Informational);
                break;
            case TransferPhase.Paused:
                ShowBanner($"Paused {item.FileName}.", NoticeSeverity.Informational);
                break;
            case TransferPhase.Cancelled:
                ShowBanner($"Cancelled {item.FileName}.", NoticeSeverity.Informational);
                break;
            case TransferPhase.Failed:
                ShowBanner(
                    $"{(item.Direction == TransferDirection.Send ? "Could not send" : "Could not receive")} " +
                    $"{item.FileName}: {item.Error ?? "the transfer failed."}",
                    NoticeSeverity.Error);
                break;
        }
    }

    private void OnStagedFilesChanged(object? sender, NotifyCollectionChangedEventArgs args)
    {
        HasStagedFiles = StagedFiles.Count > 0;

        if (StagedFiles.Count == 0)
        {
            StagedSummaryText = "No files staged";
        }
        else
        {
            var total = StagedFiles.Sum(file => file.Size);
            StagedSummaryText = StagedFiles.Count == 1
                ? $"{StagedFiles[0].Name} \u2014 {Format.Bytes(total)}"
                : $"{StagedFiles.Count} files \u2014 {Format.Bytes(total)}";
        }

        OnPropertiesChanged(nameof(SendButtonText), nameof(CanSend));
    }

    // -------------------------------------------------------------- discovery

    private void ApplyPeers(DiscoveryPeersEventArgs args)
    {
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);

        foreach (var peer in args.Peers)
        {
            seen.Add(peer.IdHex);
            if (_peerIndex.TryGetValue(peer.IdHex, out var existing))
            {
                existing.Update(peer, _settings.IsTrusted(peer.IdHex));
                continue;
            }

            var device = new PeerDevice(peer, _settings.IsTrusted(peer.IdHex));
            _peerIndex[peer.IdHex] = device;
            InsertPeerSorted(device);
        }

        foreach (var lost in _peerIndex.Keys.Where(id => !seen.Contains(id)).ToArray())
        {
            if (_peerIndex.Remove(lost, out var device))
            {
                Peers.Remove(device);

                if (ReferenceEquals(SelectedPeer, device))
                {
                    SelectPeer(null);
                }
            }
        }

        HasPeers = Peers.Count > 0;
        PeerCountText = Peers.Count switch
        {
            0 => "None nearby",
            1 => "1 device nearby",
            _ => $"{Peers.Count} devices nearby",
        };

        if (args.Error is not null)
        {
            StatusText = "Discovery problem";
            StatusColor = "#FFFF6B7A";
            StatusDetail = args.Error;
            IsScanning = false;
        }
        else if (args.Starting)
        {
            StatusText = "Starting";
            StatusColor = "#FF9FE7FF";
            StatusDetail = "Waking up the radar\u2026";
            IsScanning = true;
        }
        else if (args.Running)
        {
            StatusText = "Scanning";
            StatusColor = "#FF6EE7C8";
            StatusDetail = Peers.Count == 0
                ? "No devices yet \u2014 keep both devices on the same Wi-Fi"
                : PeerCountText;
            IsScanning = true;
        }
        else
        {
            StatusText = "Offline";
            StatusColor = "#FF9DB2C4";
            StatusDetail = "Discovery is stopped";
            IsScanning = false;
        }
    }

    /// <summary>The launch state when automatic scanning is switched off.</summary>
    private void SetIdleState()
    {
        StatusText = "Idle";
        StatusColor = "#FF9DB2C4";
        StatusDetail = "Auto-scan is off \u2014 press Rescan to look for nearby devices";
        IsScanning = false;
        ScanNote = "Auto-scan is off. Rescan starts Bluetooth, mDNS and the subnet sweep.";
    }

    private void InsertPeerSorted(PeerDevice device)
    {
        for (var index = 0; index < Peers.Count; index++)
        {
            if (string.Compare(Peers[index].Name, device.Name, StringComparison.CurrentCultureIgnoreCase) > 0)
            {
                Peers.Insert(index, device);
                return;
            }
        }

        Peers.Add(device);
    }

    private void ApplyTier(DiscoveryTierEventArgs args)
    {
        var tier = Tiers.FirstOrDefault(candidate => candidate.Tier == args.Tier);
        tier?.Apply(args.State);
    }

    private void ApplyTierError(DiscoveryTierErrorEventArgs args)
    {
        var tier = Tiers.FirstOrDefault(candidate => candidate.Tier == args.Tier);
        if (tier is not null)
        {
            tier.Detail = args.Message;
        }

        if (args.Tier == DiscoveryTier.Ble)
        {
            ScanNote = $"Bluetooth discovery is limited: {args.Message}";
        }
    }

    private void ApplyPeerId(string peerId)
    {
        if (string.IsNullOrWhiteSpace(peerId) || string.Equals(peerId, _settings.PeerIdHex, StringComparison.OrdinalIgnoreCase))
        {
            return;
        }

        _settings.PeerIdHex = peerId;
        _store.Save(_settings);
        OnPropertyChanged(nameof(PeerIdText));
    }

    private void UpdateReceiverStatus()
    {
        if (!_settings.ReceiveEnabled)
        {
            ReceiverStatusText = "Off";
            ReceiverStatusColor = "#FF9DB2C4";
            return;
        }

        if (_coordinator.IsReceiverListening)
        {
            ReceiverStatusText = "Listening";
            ReceiverStatusColor = "#FF6EE7C8";
        }
        else
        {
            ReceiverStatusText = "Starting the listener\u2026";
            ReceiverStatusColor = "#FF9FE7FF";
        }
    }

    // ------------------------------------------------------------------ peers

    public void SelectPeer(PeerDevice? peer)
    {
        if (ReferenceEquals(SelectedPeer, peer))
        {
            return;
        }

        if (SelectedPeer is not null)
        {
            SelectedPeer.IsSelected = false;
        }

        SelectedPeer = peer;
        if (peer is not null)
        {
            peer.IsSelected = true;
        }
    }

    // ------------------------------------------------------------------ files

    public async Task AddFilesAsync(IEnumerable<StorageFile> files)
    {
        var added = 0;

        foreach (var file in files)
        {
            try
            {
                var staged = await StagedFile.FromStorageFileAsync(file);
                if (StagedFiles.Any(existing =>
                        string.Equals(existing.Name, staged.Name, StringComparison.OrdinalIgnoreCase) &&
                        existing.Size == staged.Size))
                {
                    continue;
                }

                StagedFiles.Add(staged);
                added++;
            }
            catch (Exception error)
            {
                AppLog.Warn($"Could not stage {file.Name}.", error);
                ShowBanner($"Could not read {file.Name}.", NoticeSeverity.Warning);
            }
        }

        if (added > 0)
        {
            ShowBanner($"{added} file{(added == 1 ? string.Empty : "s")} ready to send.", NoticeSeverity.Success);
        }
        else if (files.Any())
        {
            ShowBanner("Those files are already in the list.", NoticeSeverity.Informational);
        }
    }

    public async Task AddFilePathsAsync(IEnumerable<string> paths)
    {
        var files = new List<StorageFile>();

        foreach (var path in paths)
        {
            try
            {
                if (Directory.Exists(path))
                {
                    ShowBanner("Folders cannot be sent yet \u2014 pick individual files.", NoticeSeverity.Warning);
                    continue;
                }

                var file = await StorageFile.GetFileFromPathAsync(path);
                files.Add(file);
            }
            catch (Exception error)
            {
                AppLog.Warn($"Could not open {path}.", error);
            }
        }

        await AddFilesAsync(files);
    }

    public void RemoveFile(StagedFile file) => StagedFiles.Remove(file);

    public void ClearFiles() => StagedFiles.Clear();

    public async Task PickFilesAsync()
    {
        var picker = FilePicker;
        if (picker is null)
        {
            return;
        }

        try
        {
            var files = await picker();
            if (files.Count > 0)
            {
                await AddFilesAsync(files);
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("The file picker failed.", error);
        }
    }

    // -------------------------------------------------------------- transfers

    public async Task<string?> SendSelectedAsync()
    {
        var peer = SelectedPeer;
        var files = StagedFiles.ToArray();

        if (peer is null)
        {
            ShowBanner("Pick a nearby device first.", NoticeSeverity.Warning);
            return null;
        }

        if (files.Length == 0)
        {
            ShowBanner("Choose at least one file to send.", NoticeSeverity.Warning);
            return null;
        }

        if (IsBusy)
        {
            return null;
        }

        if (!peer.HasAddress)
        {
            ShowBanner($"{peer.Name} has no reachable address yet. Wait for Wi-Fi discovery.", NoticeSeverity.Warning);
            return null;
        }

        IsBusy = true;
        try
        {
            var outgoing = new List<OutgoingFile>(files.Length);
            foreach (var file in files)
            {
                try
                {
                    var path = await file.EnsureLocalPathAsync();
                    outgoing.Add(new OutgoingFile(path, file.Name, file.Size));
                }
                catch (Exception error)
                {
                    ShowBanner($"Could not read {file.Name}: {Format.FriendlyException(error)}", NoticeSeverity.Error);
                    return null;
                }
            }

            if (!await EnsureVerifiedAsync(peer))
            {
                return null;
            }

            StagedFiles.Clear();
            _coordinator.EnqueueSend(peer, outgoing);
            ScrollTransfersIntoView?.Invoke(this, EventArgs.Empty);

            var summary = outgoing.Count == 1 ? outgoing[0].Name : $"{outgoing.Count} files";
            ShowBanner($"Sending {summary} to {peer.Name}.", NoticeSeverity.Informational);
            return peer.Name;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>
    /// Re-runs the SAS handshake with the selected trusted device. Approving it
    /// replaces the pinned fingerprint if the other side legitimately rotated
    /// its key; declining leaves the existing trust unchanged.
    /// </summary>
    public async Task VerifySelectedPeerAsync()
    {
        var peer = SelectedPeer;
        if (peer is null || !peer.HasAddress || IsBusy || !NativeReady)
        {
            return;
        }

        IsBusy = true;
        try
        {
            await EnsureVerifiedAsync(peer, force: true);
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>
    /// Runs a real pairing handshake with <paramref name="peer"/> and stores
    /// trust against the fingerprint of its identity key. Trust is never keyed
    /// by the discovered peer id alone.
    /// </summary>
    private async Task<bool> EnsureVerifiedAsync(PeerDevice peer, bool force = false)
    {
        if (!force && _settings.IsTrusted(peer.IdHex))
        {
            peer.IsTrusted = true;
            return true;
        }

        if (!peer.HasAddress)
        {
            ShowBanner($"{peer.Name} has no reachable address yet.", NoticeSeverity.Warning);
            return false;
        }

        if (!TryEnsureIdentity() || _identity is null)
        {
            ShowBanner("The device identity is not ready yet. Restart AirBridge and try again.", NoticeSeverity.Error);
            return false;
        }

        var identity = _identity;
        PairingSession? session = null;
        try
        {
            session = new PairingSession(
                peer.PrimaryAddress,
                identity,
                _settings.PeerIdHex ?? string.Empty,
                PairingTimeoutMs);

            var challenge = await session.Start();
            var handler = VerificationPrompt;
            var prompt = new SasPrompt(
                challenge.Sas,
                challenge.PeerFingerprint,
                peer.Name,
                $"{peer.DeviceLabel} \u00B7 {peer.AddressText}",
                isIncoming: false,
                glyph: peer.Glyph);

            if (handler is null || !await handler(prompt))
            {
                await RejectPairingAsync(session);
                ShowBanner("Verification cancelled \u2014 nothing was sent.", NoticeSeverity.Warning);
                return false;
            }

            var result = await session.Confirm();
            if (!string.Equals(result.PeerId, peer.IdHex, StringComparison.OrdinalIgnoreCase))
            {
                ShowBanner(
                    "The device answered with a different identity. Trust was not saved.",
                    NoticeSeverity.Warning);
                return false;
            }

            _settings.Trust(result.PeerId, result.PeerFingerprint);
            _store.Save(_settings);
            peer.IsTrusted = true;
            OnPropertiesChanged(
                nameof(SelectedPeerDetail),
                nameof(SendButtonText),
                nameof(CanSend));
            ShowBanner($"{peer.Name} is now a verified device.", NoticeSeverity.Success);
            return true;
        }
        catch (FfiException.VerificationRejected)
        {
            ShowBanner("The security code was declined or the pairing timed out.", NoticeSeverity.Warning);
            return false;
        }
        catch (Exception error)
        {
            AppLog.Warn("Pairing failed.", error);
            ShowBanner($"Could not verify {peer.Name}: {Format.FriendlyException(error)}", NoticeSeverity.Error);
            return false;
        }
        finally
        {
            session?.Dispose();
        }
    }

    private static async Task RejectPairingAsync(PairingSession session)
    {
        try
        {
            await session.Reject();
        }
        catch (Exception error)
        {
            AppLog.Warn("The pairing connection did not close cleanly.", error);
        }
    }

    /// <summary>Shows the SAS dialog on the UI thread for the coordinator.</summary>
    private Task<bool> PromptForVerificationAsync(SasPrompt prompt)
    {
        var handler = VerificationPrompt;
        if (handler is null)
        {
            return Task.FromResult(false);
        }

        if (_dispatcher.HasThreadAccess)
        {
            return RunPromptAsync(handler, prompt);
        }

        var completion = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (!_dispatcher.TryEnqueue(async () => completion.TrySetResult(await RunPromptAsync(handler, prompt))))
        {
            completion.TrySetResult(false);
        }

        return completion.Task;
    }

    private static async Task<bool> RunPromptAsync(Func<SasPrompt, Task<bool>> handler, SasPrompt prompt)
    {
        try
        {
            return await handler(prompt);
        }
        catch (Exception error)
        {
            AppLog.Error("The verification prompt failed.", error);
            return false;
        }
    }

    /// <summary>
    /// Applies an authenticated id-to-fingerprint binding. Only called after
    /// the user confirmed the SAS (or a previously trusted key auto-confirmed).
    /// </summary>
    private void OnPeerVerified(PeerVerifiedEventArgs args)
    {
        var wasTrusted = _settings.TrustedFingerprints.Contains(args.PeerFingerprint);
        _settings.Trust(args.PeerId, args.PeerFingerprint);
        _store.Save(_settings);

        if (_peerIndex.TryGetValue(args.PeerId, out var peer))
        {
            peer.IsTrusted = true;
        }

        OnPropertiesChanged(nameof(SelectedPeerDetail), nameof(SendButtonText), nameof(CanSend));

        if (!wasTrusted)
        {
            var name = peer?.Name ?? Format.ShortId(args.PeerId);
            ShowBanner($"{name} is now a verified device.", NoticeSeverity.Success);
        }
    }

    public void PauseTransfer(TransferItem item) => _coordinator.Pause(item);

    public void ResumeTransfer(TransferItem item) => _coordinator.Resume(item);

    public void CancelTransfer(TransferItem item) => _coordinator.Cancel(item);

    public void DismissTransfer(TransferItem item) => _coordinator.Dismiss(item);

    public void ClearFinishedTransfers() => _coordinator.ClearFinished();

    public void RevealTransfer(TransferItem item)
    {
        var path = item.DestinationPath ?? item.SourcePath;
        if (string.IsNullOrWhiteSpace(path))
        {
            return;
        }

        try
        {
            if (File.Exists(path))
            {
                Process.Start(new ProcessStartInfo("explorer.exe", $"/select,\"{path}\"") { UseShellExecute = true });
            }
            else
            {
                var directory = Path.GetDirectoryName(path);
                if (directory is not null && Directory.Exists(directory))
                {
                    OpenFolder(directory);
                }
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not open the file location.", error);
        }
    }

    // --------------------------------------------------------------- firewall

    public async Task RefreshFirewallStatusAsync()
    {
        var status = await _firewall.GetStatusAsync().ConfigureAwait(true);
        FirewallIsReady = status.IsReady;
        FirewallStatusText = status.IsReady ? "Firewall ready" : "Firewall setup needed";
        FirewallStatusColor = status.IsReady ? "#FF6EE7C8" : "#FFFFCF70";

        if (!status.IsReady)
        {
            ScanNote = "Windows Firewall may block incoming transfers \u2014 run the one-time setup.";
        }
    }

    public async Task SetupFirewallAsync()
    {
        var error = _firewall.LaunchSetup(_settings.ListenPort);
        if (error is not null)
        {
            ShowBanner(error, NoticeSeverity.Warning);
            return;
        }

        ShowBanner("Approve the Windows prompt to add the firewall rules.", NoticeSeverity.Informational);
        await Task.Delay(4_000).ConfigureAwait(true);
        await RefreshFirewallStatusAsync().ConfigureAwait(true);

        if (FirewallIsReady)
        {
            _settings.FirewallPromptDismissed = true;
            _store.Save(_settings);
            ShowBanner("Windows Firewall is open for AirBridge.", NoticeSeverity.Success);
        }
    }

    public void OpenDownloadsFolder()
    {
        try
        {
            Directory.CreateDirectory(_settings.DownloadDirectory);
            OpenFolder(_settings.DownloadDirectory);
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not open the download folder.", error);
        }
    }

    public void OpenLogsFolder()
    {
        try
        {
            var directory = Path.GetDirectoryName(AppLog.LogFilePath);
            if (directory is null)
            {
                return;
            }

            Directory.CreateDirectory(directory);
            OpenFolder(directory);
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not open the log folder.", error);
        }
    }

    private static void OpenFolder(string directory) =>
        Process.Start(new ProcessStartInfo("explorer.exe", $"\"{directory}\"") { UseShellExecute = true });

    // ---------------------------------------------------------------- banners

    public void ShowBanner(string message, NoticeSeverity severity)
    {
        BannerSeverity = severity switch
        {
            NoticeSeverity.Success => InfoBarSeverity.Success,
            NoticeSeverity.Warning => InfoBarSeverity.Warning,
            NoticeSeverity.Error => InfoBarSeverity.Error,
            _ => InfoBarSeverity.Informational,
        };

        BannerText = message;
        IsBannerOpen = true;
        AppLog.Info($"[UI] {message}");
    }

    private void Dispatch(Action action)
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
}
