using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using AirBridge.Windows.Controls;
using AirBridge.Windows.Core;
using AirBridge.Windows.Dialogs;
using AirBridge.Windows.Models;
using AirBridge.Windows.Services;
using AirBridge.Windows.ViewModels;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Graphics;
using Windows.Storage;
using Windows.Storage.Pickers;
using Color = Windows.UI.Color;

namespace AirBridge.Windows;

/// <summary>
/// The application window. It owns the view model, turns dialog results into
/// view-model calls, and is the only place where XamlRoot / window handles are
/// available to the pickers and ContentDialogs.
/// </summary>
public sealed partial class MainWindow : Window
{
    private readonly MainViewModel _viewModel;
    private readonly IntPtr _windowHandle;

    private bool _loaded;
    private bool _dialogOpen;
    private bool _closed;

    public MainWindow()
    {
        InitializeComponent();

        _windowHandle = WinRT.Interop.WindowNative.GetWindowHandle(this);
        _viewModel = new MainViewModel(DispatcherQueue, new SettingsStore());
        _viewModel.VerificationPrompt = ShowVerificationAsync;
        _viewModel.FilePicker = PickFilesAsync;
        _viewModel.ScrollTransfersIntoView += OnScrollTransfersIntoView;

        RootGrid.DataContext = _viewModel;
        RootGrid.Loaded += OnRootLoaded;
        Closed += OnClosed;

        Title = "AirBridge";
        ConfigureWindow();
    }

    private void ConfigureWindow()
    {
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(AppTitleBar);

        var titleBar = AppWindow.TitleBar;
        titleBar.ButtonBackgroundColor = Colors.Transparent;
        titleBar.ButtonInactiveBackgroundColor = Colors.Transparent;
        titleBar.ButtonForegroundColor = Color.FromArgb(0xFF, 0xEA, 0xF4, 0xFF);
        titleBar.ButtonInactiveForegroundColor = Color.FromArgb(0x80, 0x9D, 0xB2, 0xC4);
        titleBar.ButtonHoverBackgroundColor = Color.FromArgb(0x1A, 0xFF, 0xFF, 0xFF);
        titleBar.ButtonHoverForegroundColor = Colors.White;
        titleBar.ButtonPressedBackgroundColor = Color.FromArgb(0x2E, 0xFF, 0xFF, 0xFF);
        titleBar.ButtonPressedForegroundColor = Colors.White;

        var area = DisplayArea.GetFromWindowId(AppWindow.Id, DisplayAreaFallback.Primary);
        var width = Math.Min(1360, Math.Max(720, area.WorkArea.Width - 120));
        var height = Math.Min(920, Math.Max(600, area.WorkArea.Height - 100));
        var x = area.WorkArea.X + Math.Max(0, (area.WorkArea.Width - width) / 2);
        var y = area.WorkArea.Y + Math.Max(0, (area.WorkArea.Height - height) / 2);

        AppWindow.MoveAndResize(new RectInt32(x, y, width, height));
    }

    private async void OnRootLoaded(object sender, RoutedEventArgs args)
    {
        if (_loaded)
        {
            return;
        }

        _loaded = true;

        try
        {
            await _viewModel.InitializeAsync();

            if (_viewModel.NativeReady &&
                !_viewModel.FirewallIsReady &&
                !_viewModel.Settings.FirewallPromptDismissed)
            {
                _viewModel.ShowBanner(
                    "Windows Firewall may block incoming transfers. Open Sharing and choose Set up to add the rules once.",
                    NoticeSeverity.Warning);
            }
        }
        catch (Exception error)
        {
            AppLog.Error("The main window failed to initialise.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
    }

    private async void OnClosed(object sender, WindowEventArgs args)
    {
        if (_closed)
        {
            return;
        }

        _closed = true;

        try
        {
            await _viewModel.DisposeAsync();
        }
        catch (Exception error)
        {
            AppLog.Warn("The view model did not shut down cleanly.", error);
        }
    }

    private void OnScrollTransfersIntoView(object? sender, EventArgs args) =>
        DispatcherQueue.TryEnqueue(() => TransfersScrollViewer.ChangeView(null, 0, null, true));

    // -------------------------------------------------------------- dialogs

    private async Task<bool> ShowVerificationAsync(PeerDevice peer, SasChallenge challenge)
    {
        if (RootGrid.XamlRoot is null || _dialogOpen)
        {
            return false;
        }

        _dialogOpen = true;

        try
        {
            var dialog = new SasVerificationDialog(peer, challenge, _viewModel.IsPairingPreview)
            {
                XamlRoot = RootGrid.XamlRoot,
            };

            return await dialog.ShowAsync() == ContentDialogResult.Primary;
        }
        catch (Exception error)
        {
            AppLog.Error("The SAS dialog could not be shown.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
            return false;
        }
        finally
        {
            _dialogOpen = false;
        }
    }

    private async void OnSettingsClick(object sender, RoutedEventArgs args)
    {
        if (RootGrid.XamlRoot is null || _dialogOpen)
        {
            return;
        }

        _dialogOpen = true;
        var pageOffset = PageScroll.VerticalOffset;

        try
        {
            var dialog = new SettingsDialog(_viewModel.Settings, _windowHandle)
            {
                XamlRoot = RootGrid.XamlRoot,
            };

            if (await dialog.ShowAsync() == ContentDialogResult.Primary && dialog.Settings is { } updated)
            {
                await _viewModel.ApplySettingsAsync(updated);
                _viewModel.ShowBanner("Settings saved.", NoticeSeverity.Success);
            }
        }
        catch (Exception error)
        {
            AppLog.Error("The settings dialog failed.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
        finally
        {
            _dialogOpen = false;

            // Showing the dialog can make the page scroll to the focused
            // setting. Put the page back where the user left it.
            DispatcherQueue.TryEnqueue(() => PageScroll.ChangeView(null, pageOffset, null, true));
        }
    }

    // ---------------------------------------------------------------- files

    private async Task<IReadOnlyList<StorageFile>> PickFilesAsync()
    {
        var picker = new FileOpenPicker
        {
            SuggestedStartLocation = PickerLocationId.DocumentsLibrary,
            ViewMode = PickerViewMode.List,
        };

        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, _windowHandle);

        var files = await picker.PickMultipleFilesAsync();
        return files is null ? Array.Empty<StorageFile>() : files.ToArray();
    }

    private async void OnBrowseClick(object sender, RoutedEventArgs args) => await _viewModel.PickFilesAsync();

    private async void OnBrowseRequested(object sender, EventArgs args) => await _viewModel.PickFilesAsync();

    private async void OnFilesDropped(object sender, FilesDroppedEventArgs args)
    {
        try
        {
            if (args.Files.Count > 0)
            {
                await _viewModel.AddFilesAsync(args.Files);
            }

            if (args.Paths.Count > 0)
            {
                await _viewModel.AddFilePathsAsync(args.Paths);
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("The dropped files could not be staged.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
    }

    private void OnRemoveFileClick(object sender, RoutedEventArgs args)
    {
        if (sender is FrameworkElement { DataContext: StagedFile file })
        {
            _viewModel.RemoveFile(file);
        }
    }

    private void OnClearFilesClick(object sender, RoutedEventArgs args) => _viewModel.ClearFiles();

    // --------------------------------------------------------------- peers

    private void OnPeerInvoked(object sender, PeerInvokedEventArgs args) => _viewModel.SelectPeer(args.Peer);

    private void OnPeerGridItemClick(object sender, ItemClickEventArgs args)
    {
        if (args.ClickedItem is PeerDevice peer)
        {
            _viewModel.SelectPeer(peer);
        }
    }

    // ----------------------------------------------------------- transfers

    private async void OnSendClick(object sender, RoutedEventArgs args)
    {
        try
        {
            await _viewModel.SendSelectedAsync();
        }
        catch (Exception error)
        {
            AppLog.Error("Sending failed unexpectedly.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
    }

    private void OnPauseTransferClick(object sender, RoutedEventArgs args)
    {
        if (TransferFrom(sender) is { } item)
        {
            _viewModel.PauseTransfer(item);
        }
    }

    private void OnResumeTransferClick(object sender, RoutedEventArgs args)
    {
        if (TransferFrom(sender) is { } item)
        {
            _viewModel.ResumeTransfer(item);
        }
    }

    private void OnCancelTransferClick(object sender, RoutedEventArgs args)
    {
        if (TransferFrom(sender) is { } item)
        {
            _viewModel.CancelTransfer(item);
        }
    }

    private void OnDismissTransferClick(object sender, RoutedEventArgs args)
    {
        if (TransferFrom(sender) is { } item)
        {
            _viewModel.DismissTransfer(item);
        }
    }

    private void OnRevealTransferClick(object sender, RoutedEventArgs args)
    {
        if (TransferFrom(sender) is { } item)
        {
            _viewModel.RevealTransfer(item);
        }
    }

    private void OnClearFinishedClick(object sender, RoutedEventArgs args) => _viewModel.ClearFinishedTransfers();

    private static TransferItem? TransferFrom(object sender) =>
        (sender as FrameworkElement)?.DataContext as TransferItem;

    // ------------------------------------------------------------- discovery

    private async void OnRefreshClick(object sender, RoutedEventArgs args)
    {
        var button = sender as Button;
        if (button is not null)
        {
            button.IsEnabled = false;
        }

        try
        {
            await _viewModel.RefreshAsync();
        }
        catch (Exception error)
        {
            AppLog.Warn("Discovery refresh failed.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
        finally
        {
            if (button is not null)
            {
                button.IsEnabled = true;
            }
        }
    }

    // -------------------------------------------------------------- sharing

    private async void OnSetupFirewallClick(object sender, RoutedEventArgs args)
    {
        var button = sender as Button;
        if (button is not null)
        {
            button.IsEnabled = false;
        }

        try
        {
            await _viewModel.SetupFirewallAsync();
        }
        catch (Exception error)
        {
            AppLog.Warn("Firewall setup failed.", error);
            _viewModel.ShowBanner(Format.FriendlyException(error), NoticeSeverity.Error);
        }
        finally
        {
            if (button is not null)
            {
                button.IsEnabled = true;
            }
        }
    }

    private async void OnRefreshFirewallClick(object sender, RoutedEventArgs args)
    {
        try
        {
            await _viewModel.RefreshFirewallStatusAsync();
        }
        catch (Exception error)
        {
            AppLog.Warn("Firewall status refresh failed.", error);
        }
    }

    private void OnOpenDownloadsClick(object sender, RoutedEventArgs args) => _viewModel.OpenDownloadsFolder();

    private void OnOpenLogsClick(object sender, RoutedEventArgs args) => _viewModel.OpenLogsFolder();
}
