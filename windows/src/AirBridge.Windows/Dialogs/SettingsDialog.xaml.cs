using System;
using System.IO;
using AirBridge.Core;
using AirBridge.Windows.Core;
using AirBridge.Windows.Services;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage.Pickers;

namespace AirBridge.Windows.Dialogs;

/// <summary>
/// Edits a clone of the persisted settings. The dialog only produces a new
/// <see cref="AppSettings"/>; applying it (and restarting discovery/receiver)
/// is the main window's job.
/// </summary>
public sealed partial class SettingsDialog : ContentDialog
{
    private readonly IntPtr _windowHandle;
    private readonly AppSettings _original;

    public SettingsDialog(AppSettings current, IntPtr windowHandle)
    {
        ArgumentNullException.ThrowIfNull(current);

        InitializeComponent();

        _original = current;
        _windowHandle = windowHandle;

        DisplayNameBox.Text = current.DisplayName;
        PortBox.Value = current.ListenPort;
        DownloadBox.Text = current.DownloadDirectory;
        ReceiveToggle.IsOn = current.ReceiveEnabled;
        AutoScanToggle.IsOn = current.AutoScanOnLaunch;
        BleToggle.IsOn = current.EnableBle;
        MdnsToggle.IsOn = current.EnableMdns;
        SubnetBox.SelectedIndex = current.SubnetPolicy switch
        {
            SubnetPolicy.Disabled => 0,
            SubnetPolicy.Always => 2,
            _ => 1,
        };

        StorageNoteText.Text =
            $"Settings and logs: {Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AirBridge")}";
        PeerIdNoteText.Text = string.IsNullOrWhiteSpace(current.PeerIdHex)
            ? "Peer ID: assigned on the first scan"
            : $"Peer ID: {current.PeerIdHex}";
    }

    /// <summary>The edited settings, valid after the user chooses Save.</summary>
    public AppSettings? Settings { get; private set; }

    private void OnPrimaryButtonClick(ContentDialog sender, ContentDialogButtonClickEventArgs args)
    {
        var name = DisplayNameBox.Text?.Trim();
        if (string.IsNullOrWhiteSpace(name))
        {
            Reject(args, "Give this device a name so other people can recognise it.");
            return;
        }

        var portValue = PortBox.Value;
        if (double.IsNaN(portValue) || portValue < 1024 || portValue > 65535)
        {
            Reject(args, "The transfer port must be between 1024 and 65535.");
            return;
        }

        var directory = DownloadBox.Text?.Trim();
        if (string.IsNullOrWhiteSpace(directory))
        {
            Reject(args, "Choose a folder for received files.");
            return;
        }

        try
        {
            directory = Path.GetFullPath(directory);
            Directory.CreateDirectory(directory);
        }
        catch (Exception error)
        {
            Reject(args, $"That download folder cannot be used: {Format.FriendlyException(error)}");
            return;
        }

        Settings = _original.Clone();
        Settings.DisplayName = name;
        Settings.ListenPort = (ushort)Math.Round(portValue);
        Settings.DownloadDirectory = directory;
        Settings.ReceiveEnabled = ReceiveToggle.IsOn;
        Settings.AutoScanOnLaunch = AutoScanToggle.IsOn;
        Settings.EnableBle = BleToggle.IsOn;
        Settings.EnableMdns = MdnsToggle.IsOn;
        Settings.SubnetPolicy = SubnetBox.SelectedIndex switch
        {
            0 => SubnetPolicy.Disabled,
            2 => SubnetPolicy.Always,
            _ => SubnetPolicy.Fallback,
        };

        ErrorText.Visibility = Visibility.Collapsed;
    }

    private async void OnBrowseFolder(object sender, RoutedEventArgs args)
    {
        try
        {
            var picker = new FolderPicker
            {
                SuggestedStartLocation = PickerLocationId.Downloads,
            };
            picker.FileTypeFilter.Add("*");

            WinRT.Interop.InitializeWithWindow.Initialize(picker, _windowHandle);

            var folder = await picker.PickSingleFolderAsync();
            if (folder is not null)
            {
                DownloadBox.Text = folder.Path;
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("The folder picker failed.", error);
            ErrorText.Text = Format.FriendlyException(error);
            ErrorText.Visibility = Visibility.Visible;
        }
    }

    private void Reject(ContentDialogButtonClickEventArgs args, string message)
    {
        args.Cancel = true;
        ErrorText.Text = message;
        ErrorText.Visibility = Visibility.Visible;
    }
}
