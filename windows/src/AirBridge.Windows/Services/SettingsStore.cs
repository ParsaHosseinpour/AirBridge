using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;
using AirBridge.Core;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Services;

/// <summary>Persisted, user-visible configuration. Everything here is local.</summary>
public sealed class AppSettings
{
    public string DisplayName { get; set; } = DefaultDeviceName();
    public string? PeerIdHex { get; set; }
    public ushort ListenPort { get; set; } = 42517;
    public bool EnableBle { get; set; } = true;
    public bool EnableMdns { get; set; } = true;
    public SubnetPolicy SubnetPolicy { get; set; } = AirBridge.Core.SubnetPolicy.Fallback;
    public bool ReceiveEnabled { get; set; } = true;
    public string DownloadDirectory { get; set; } = DefaultDownloadDirectory();
    public HashSet<string> TrustedPeers { get; set; } = new(StringComparer.OrdinalIgnoreCase);
    public bool FirewallPromptDismissed { get; set; }
    public bool AutoScanOnLaunch { get; set; } = true;

    public static string DefaultDeviceName()
    {
        var user = Environment.UserName.Replace('.', ' ').Replace('_', ' ').Trim();
        if (string.IsNullOrWhiteSpace(user))
        {
            user = Environment.MachineName;
        }

        var title = CultureInfo.CurrentCulture.TextInfo.ToTitleCase(user.ToLowerInvariant());
        return string.IsNullOrWhiteSpace(title) ? "My PC" : $"{title}'s PC";
    }

    public static string DefaultDownloadDirectory() => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.UserProfile),
        "Downloads",
        "AirBridge");

    public bool IsTrusted(string peerIdHex) =>
        !string.IsNullOrWhiteSpace(peerIdHex) && TrustedPeers.Contains(peerIdHex);

    public void Trust(string peerIdHex)
    {
        if (!string.IsNullOrWhiteSpace(peerIdHex))
        {
            TrustedPeers.Add(peerIdHex);
        }
    }

    public void Forget(string peerIdHex)
    {
        if (!string.IsNullOrWhiteSpace(peerIdHex))
        {
            TrustedPeers.Remove(peerIdHex);
        }
    }

    public AppSettings Clone() => new()
    {
        DisplayName = DisplayName,
        PeerIdHex = PeerIdHex,
        ListenPort = ListenPort,
        EnableBle = EnableBle,
        EnableMdns = EnableMdns,
        SubnetPolicy = SubnetPolicy,
        ReceiveEnabled = ReceiveEnabled,
        DownloadDirectory = DownloadDirectory,
        TrustedPeers = new HashSet<string>(TrustedPeers, StringComparer.OrdinalIgnoreCase),
        FirewallPromptDismissed = FirewallPromptDismissed,
        AutoScanOnLaunch = AutoScanOnLaunch,
    };
}

/// <summary>Reads and writes <see cref="AppSettings"/> next to the user's local data.</summary>
public sealed class SettingsStore
{
    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        WriteIndented = true,
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        Converters = { new JsonStringEnumConverter() },
    };

    private readonly object _sync = new();
    private readonly string _path;

    public SettingsStore()
    {
        var directory = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "AirBridge");
        Directory.CreateDirectory(directory);
        _path = Path.Combine(directory, "settings.json");
    }

    public string FilePath => _path;

    public AppSettings Load()
    {
        lock (_sync)
        {
            try
            {
                if (!File.Exists(_path))
                {
                    return Normalize(new AppSettings());
                }

                var json = File.ReadAllText(_path);
                var settings = JsonSerializer.Deserialize<AppSettings>(json, JsonOptions) ?? new AppSettings();
                return Normalize(settings);
            }
            catch (Exception error)
            {
                AppLog.Warn("Could not read settings; using defaults.", error);
                return Normalize(new AppSettings());
            }
        }
    }

    public void Save(AppSettings settings)
    {
        lock (_sync)
        {
            try
            {
                var directory = Path.GetDirectoryName(_path);
                if (!string.IsNullOrEmpty(directory))
                {
                    Directory.CreateDirectory(directory);
                }

                var json = JsonSerializer.Serialize(settings, JsonOptions);
                var temporary = _path + ".tmp";
                File.WriteAllText(temporary, json);
                File.Move(temporary, _path, overwrite: true);
            }
            catch (Exception error)
            {
                AppLog.Error("Could not save settings.", error);
            }
        }
    }

    private static AppSettings Normalize(AppSettings settings)
    {
        settings.DisplayName = string.IsNullOrWhiteSpace(settings.DisplayName)
            ? AppSettings.DefaultDeviceName()
            : settings.DisplayName.Trim();

        if (settings.ListenPort < 1024)
        {
            settings.ListenPort = 42517;
        }

        if (string.IsNullOrWhiteSpace(settings.DownloadDirectory))
        {
            settings.DownloadDirectory = AppSettings.DefaultDownloadDirectory();
        }

        // HashSet comparers are not part of the JSON representation.
        settings.TrustedPeers = new HashSet<string>(
            settings.TrustedPeers.Where(id => !string.IsNullOrWhiteSpace(id)),
            StringComparer.OrdinalIgnoreCase);

        try
        {
            Directory.CreateDirectory(settings.DownloadDirectory);
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not create the download directory.", error);
            settings.DownloadDirectory = AppSettings.DefaultDownloadDirectory();
        }

        return settings;
    }
}
