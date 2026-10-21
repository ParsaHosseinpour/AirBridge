using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Security.Cryptography;
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

    /// <summary>
    /// SHA-256 fingerprints of identity keys the user verified. A peer id is
    /// only a discovery handle; trust is keyed by the fingerprint so a device
    /// that spoofs its advertised id cannot inherit another device's trust.
    /// </summary>
    public HashSet<string> TrustedFingerprints { get; set; } = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>Authenticated peer-id to key-fingerprint bindings.</summary>
    public Dictionary<string, string> PeerFingerprints { get; set; } = new(StringComparer.OrdinalIgnoreCase);

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

    public static bool IsValidPeerId(string? peerIdHex) =>
        peerIdHex is { Length: 32 } && peerIdHex.All(IsHexCharacter);

    public static bool IsValidFingerprint(string? fingerprintHex) =>
        fingerprintHex is { Length: 64 } && fingerprintHex.All(IsHexCharacter);

    public static string GeneratePeerId()
    {
        Span<byte> bytes = stackalloc byte[16];
        RandomNumberGenerator.Fill(bytes);
        return Convert.ToHexString(bytes).ToLowerInvariant();
    }

    /// <summary>True when this id is bound to a fingerprint the user verified.</summary>
    public bool IsTrusted(string peerIdHex)
    {
        if (!IsValidPeerId(peerIdHex) ||
            !PeerFingerprints.TryGetValue(peerIdHex, out var fingerprint))
        {
            return false;
        }

        return IsValidFingerprint(fingerprint) && TrustedFingerprints.Contains(fingerprint);
    }

    /// <summary>Fingerprint the peer id was last authenticated against.</summary>
    public string? PeerFingerprint(string peerIdHex) =>
        IsValidPeerId(peerIdHex) && PeerFingerprints.TryGetValue(peerIdHex, out var fingerprint) &&
        IsValidFingerprint(fingerprint)
            ? fingerprint.ToLowerInvariant()
            : null;

    /// <summary>Peer id previously bound to a fingerprint, for display only.</summary>
    public string? PeerIdForFingerprint(string fingerprintHex)
    {
        if (!IsValidFingerprint(fingerprintHex))
        {
            return null;
        }

        foreach (var pair in PeerFingerprints)
        {
            if (string.Equals(pair.Value, fingerprintHex, StringComparison.OrdinalIgnoreCase))
            {
                return pair.Key;
            }
        }

        return null;
    }

    /// <summary>
    /// Records an authenticated id-to-fingerprint binding without granting
    /// trust yet. A key belongs to one device, so stale mappings that point a
    /// different id at the same fingerprint are dropped.
    /// </summary>
    public void BindPeer(string peerIdHex, string fingerprintHex)
    {
        if (!IsValidPeerId(peerIdHex) || !IsValidFingerprint(fingerprintHex))
        {
            return;
        }

        var id = peerIdHex.ToLowerInvariant();
        var fingerprint = fingerprintHex.ToLowerInvariant();

        foreach (var stale in PeerFingerprints
                     .Where(pair =>
                         !string.Equals(pair.Key, id, StringComparison.OrdinalIgnoreCase) &&
                         string.Equals(pair.Value, fingerprint, StringComparison.OrdinalIgnoreCase))
                     .Select(pair => pair.Key)
                     .ToArray())
        {
            PeerFingerprints.Remove(stale);
        }

        PeerFingerprints[id] = fingerprint;
    }

    /// <summary>Binds an authenticated peer id and marks its key as trusted.</summary>
    public void Trust(string peerIdHex, string fingerprintHex)
    {
        if (!IsValidPeerId(peerIdHex) || !IsValidFingerprint(fingerprintHex))
        {
            return;
        }

        var id = peerIdHex.ToLowerInvariant();
        var fingerprint = fingerprintHex.ToLowerInvariant();
        var previous = PeerFingerprints.TryGetValue(id, out var existing) ? existing : null;

        BindPeer(id, fingerprint);
        TrustedFingerprints.Add(fingerprint);

        if (previous is not null &&
            !string.Equals(previous, fingerprint, StringComparison.OrdinalIgnoreCase) &&
            !PeerFingerprints.Values.Any(value =>
                string.Equals(value, previous, StringComparison.OrdinalIgnoreCase)))
        {
            // Verifying a replacement key for this peer supersedes the old
            // key; otherwise a stale trusted key could auto-confirm later.
            TrustedFingerprints.Remove(previous);
        }
    }

    /// <summary>Removes the peer's binding and revokes trust in its key.</summary>
    public void Forget(string peerIdHex)
    {
        if (!IsValidPeerId(peerIdHex) ||
            !PeerFingerprints.Remove(peerIdHex.ToLowerInvariant(), out var fingerprint))
        {
            return;
        }

        if (!PeerFingerprints.Values.Any(value =>
                string.Equals(value, fingerprint, StringComparison.OrdinalIgnoreCase)))
        {
            TrustedFingerprints.Remove(fingerprint);
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
        TrustedFingerprints = new HashSet<string>(TrustedFingerprints, StringComparer.OrdinalIgnoreCase),
        PeerFingerprints = new Dictionary<string, string>(PeerFingerprints, StringComparer.OrdinalIgnoreCase),
        FirewallPromptDismissed = FirewallPromptDismissed,
        AutoScanOnLaunch = AutoScanOnLaunch,
    };

    private static bool IsHexCharacter(char character) =>
        character is >= '0' and <= '9' or >= 'a' and <= 'f' or >= 'A' and <= 'F';
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
    private readonly string _identityPath;

    public SettingsStore()
    {
        var directory = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "AirBridge");
        Directory.CreateDirectory(directory);
        _path = Path.Combine(directory, "settings.json");
        _identityPath = Path.Combine(directory, "identity.pkcs8");
    }

    public string FilePath => _path;

    public string IdentityPath => _identityPath;

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

                // `trustedPeers` from older builds is intentionally ignored:
                // an unauthenticated peer id proves nothing about identity.
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

    /// <summary>
    /// Reads the stored PKCS#8 identity. Returns null when none exists yet; the
    /// caller validates and creates one once the native core is available.
    /// </summary>
    public byte[]? LoadIdentity()
    {
        lock (_sync)
        {
            try
            {
                return File.Exists(_identityPath) ? File.ReadAllBytes(_identityPath) : null;
            }
            catch (Exception error)
            {
                AppLog.Warn("Could not read the device identity.", error);
                return null;
            }
        }
    }

    /// <summary>Persists the PKCS#8 identity. The bytes are secret.</summary>
    public void SaveIdentity(byte[] pkcs8)
    {
        ArgumentNullException.ThrowIfNull(pkcs8);

        lock (_sync)
        {
            try
            {
                var directory = Path.GetDirectoryName(_identityPath);
                if (!string.IsNullOrEmpty(directory))
                {
                    Directory.CreateDirectory(directory);
                }

                var temporary = _identityPath + ".tmp";
                File.WriteAllBytes(temporary, pkcs8);
                File.Move(temporary, _identityPath, overwrite: true);
            }
            catch (Exception error)
            {
                AppLog.Error("Could not save the device identity.", error);
            }
        }
    }

    private static AppSettings Normalize(AppSettings settings)
    {
        settings.DisplayName = string.IsNullOrWhiteSpace(settings.DisplayName)
            ? AppSettings.DefaultDeviceName()
            : settings.DisplayName.Trim();

        if (!AppSettings.IsValidPeerId(settings.PeerIdHex))
        {
            settings.PeerIdHex = AppSettings.GeneratePeerId();
        }
        else
        {
            settings.PeerIdHex = settings.PeerIdHex!.ToLowerInvariant();
        }

        if (settings.ListenPort < 1024)
        {
            settings.ListenPort = 42517;
        }

        if (string.IsNullOrWhiteSpace(settings.DownloadDirectory))
        {
            settings.DownloadDirectory = AppSettings.DefaultDownloadDirectory();
        }

        // HashSet/Dictionary comparers are not part of the JSON representation.
        settings.TrustedFingerprints = new HashSet<string>(
            (settings.TrustedFingerprints ?? new HashSet<string>())
            .Where(AppSettings.IsValidFingerprint)
            .Select(fingerprint => fingerprint.ToLowerInvariant()),
            StringComparer.OrdinalIgnoreCase);

        var bindings = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        foreach (var pair in settings.PeerFingerprints ?? new Dictionary<string, string>())
        {
            if (!AppSettings.IsValidPeerId(pair.Key) || !AppSettings.IsValidFingerprint(pair.Value))
            {
                continue;
            }

            bindings[pair.Key.ToLowerInvariant()] = pair.Value.ToLowerInvariant();
        }

        settings.PeerFingerprints = bindings;

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
