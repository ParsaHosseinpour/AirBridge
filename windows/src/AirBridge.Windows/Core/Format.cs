using System;
using System.Globalization;
using System.IO;
using System.Linq;

namespace AirBridge.Windows.Core;

public static class Format
{
    private static readonly string[] ByteUnits = { "bytes", "KB", "MB", "GB", "TB" };

    public static string Bytes(ulong bytes) => Bytes((long)Math.Min(bytes, long.MaxValue));

    public static string Bytes(long bytes)
    {
        if (bytes <= 0)
        {
            return "0 bytes";
        }

        double value = bytes;
        var unit = 0;
        while (value >= 1024 && unit < ByteUnits.Length - 1)
        {
            value /= 1024;
            unit++;
        }

        var pattern = value >= 100 ? "0" : value >= 10 ? "0.0" : "0.00";
        return string.Format(CultureInfo.InvariantCulture, "{0:" + pattern + "} {1}", value, ByteUnits[unit]);
    }

    public static string Rate(double bytesPerSecond)
    {
        if (double.IsNaN(bytesPerSecond) || bytesPerSecond <= 0)
        {
            return "—";
        }

        return Bytes((long)Math.Min(bytesPerSecond, long.MaxValue)) + "/s";
    }

    public static string Eta(double seconds)
    {
        if (double.IsNaN(seconds) || double.IsInfinity(seconds) || seconds < 0)
        {
            return "—";
        }

        if (seconds < 1)
        {
            return "less than a second";
        }

        if (seconds < 60)
        {
            return $"{Math.Ceiling(seconds):0}s left";
        }

        if (seconds < 3600)
        {
            return $"{seconds / 60:0}m {seconds % 60:00}s left";
        }

        return $"{seconds / 3600:0}h {(seconds % 3600) / 60:00}m left";
    }

    public static string Initials(string? name)
    {
        if (string.IsNullOrWhiteSpace(name))
        {
            return "?";
        }

        var parts = name.Split(' ', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries);
        return parts.Length switch
        {
            0 => "?",
            1 => parts[0].Length >= 2 ? parts[0][..2].ToUpperInvariant() : parts[0].ToUpperInvariant(),
            _ => string.Concat(parts[0][0], parts[1][0]).ToUpperInvariant(),
        };
    }

    public static string ShortId(string idHex) =>
        string.IsNullOrWhiteSpace(idHex) ? "UNKNOWN" : idHex[..Math.Min(8, idHex.Length)].ToUpperInvariant();

    public static string DisplayName(string? advertised, string idHex)
    {
        var trimmed = advertised?.Trim();
        return string.IsNullOrWhiteSpace(trimmed)
            ? $"Device {ShortId(idHex)}"
            : trimmed;
    }

    public static string SanitizeFileName(string name, string fallback = "airbridge-file")
    {
        var cleaned = new string(name
            .Select(character => Path.GetInvalidFileNameChars().Contains(character) ? '_' : character)
            .ToArray())
            .Trim()
            .TrimEnd('.');

        if (cleaned.Length > 120)
        {
            cleaned = cleaned[..120];
        }

        return string.IsNullOrWhiteSpace(cleaned) ? fallback : cleaned;
    }

    public static string FriendlyException(Exception error) => error switch
    {
        AirBridge.Core.FfiException.InvalidArgument invalid => invalid.@message,
        AirBridge.Core.FfiException.Timeout timeout => timeout.@message,
        AirBridge.Core.FfiException.Protocol protocol => $"Protocol error: {protocol.@message}",
        AirBridge.Core.FfiException.Io io => $"Network or file error: {io.@message}",
        AirBridge.Core.FfiException.ChecksumMismatch checksum => $"Integrity check failed: {checksum.@message}",
        AirBridge.Core.FfiException.PeerAborted => "The other device aborted the transfer.",
        AirBridge.Core.FfiException.PeerClosed => "The other device disconnected.",
        AirBridge.Core.FfiException.Cancelled => "The transfer was cancelled.",
        AirBridge.Core.FfiException.NotStarted => "Discovery is not running.",
        AirBridge.Core.FfiException.AlreadyStarted => "Discovery is already running.",
        AirBridge.Core.FfiException.Discovery discovery => discovery.@message,
        AirBridge.Core.FfiException.Internal internalError => $"Internal error: {internalError.@message}",
        DllNotFoundException => "The native AirBridge core (core_ffi.dll) could not be found.",
        BadImageFormatException => "The native AirBridge core does not match this app's architecture.",
        UnauthorizedAccessException unauthorized => $"Access denied: {unauthorized.Message}",
        IOException io => io.Message,
        _ => error.Message,
    };
}
