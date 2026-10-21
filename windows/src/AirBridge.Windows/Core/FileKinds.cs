using System;
using System.IO;
using System.Linq;

namespace AirBridge.Windows.Core;

/// <summary>
/// Best-effort file-type detection for received payloads.
///
/// The AirBridge transport moves one anonymous byte stream and carries no
/// filename, so a received file arrives without an extension. Sniffing the
/// first bytes gives the user something double-clickable without pretending
/// the network told us anything it did not.
/// </summary>
public static class FileKinds
{
    public static string SniffExtension(string path)
    {
        Span<byte> header = stackalloc byte[512];
        var length = 0;

        try
        {
            using var stream = File.OpenRead(path);
            length = stream.Read(header);
        }
        catch
        {
            return ".bin";
        }

        if (length == 0)
        {
            return ".bin";
        }

        header = header[..length];

        if (StartsWith(header, 0x89, 0x50, 0x4E, 0x47)) return ".png";
        if (StartsWith(header, 0xFF, 0xD8, 0xFF)) return ".jpg";
        if (StartsWith(header, 0x47, 0x49, 0x46, 0x38)) return ".gif";
        if (StartsWith(header, 0x25, 0x50, 0x44, 0x46)) return ".pdf";
        if (StartsWith(header, 0x50, 0x4B, 0x03, 0x04) || StartsWith(header, 0x50, 0x4B, 0x05, 0x06)) return ".zip";
        if (StartsWith(header, 0x52, 0x61, 0x72, 0x21)) return ".rar";
        if (StartsWith(header, 0x37, 0x7A, 0xBC, 0xAF)) return ".7z";
        if (StartsWith(header, 0x66, 0x4C, 0x61, 0x43)) return ".flac";
        if (StartsWith(header, 0x4F, 0x67, 0x67, 0x53)) return ".ogg";
        if (StartsWith(header, 0x1A, 0x45, 0xDF, 0xA3)) return ".mkv";
        if (StartsWith(header, 0x49, 0x44, 0x33)) return ".mp3";
        if (header.Length >= 2 && header[0] == 0xFF && (header[1] & 0xE0) == 0xE0) return ".mp3";
        if (StartsWith(header, 0x52, 0x49, 0x46, 0x46) && header.Length >= 12 &&
            header[8] == (byte)'W' && header[9] == (byte)'A' && header[10] == (byte)'V' && header[11] == (byte)'E')
        {
            return ".wav";
        }

        if (header.Length >= 12 && header[4] == (byte)'f' && header[5] == (byte)'t' &&
            header[6] == (byte)'y' && header[7] == (byte)'p')
        {
            var brand = System.Text.Encoding.ASCII.GetString(header[8..12]);
            return brand switch
            {
                "heic" or "heix" or "heif" or "mif1" => ".heic",
                "qt  " => ".mov",
                "avif" => ".avif",
                _ => ".mp4",
            };
        }

        if (IsProbablyText(header))
        {
            return ".txt";
        }

        return ".bin";
    }

    public static string BuildFriendlyName(ulong payloadId, DateTime timestamp, string extension)
    {
        var stamp = timestamp.ToString("yyyy-MM-dd HH-mm-ss");
        return $"AirBridge {stamp} {payloadId:x8}{extension}";
    }

    /// <summary>Segoe MDL2 glyph for a file category, shared by the file lists.</summary>
    public static string GlyphFor(string name)
    {
        var extension = Path.GetExtension(name).ToLowerInvariant();
        return extension switch
        {
            ".png" or ".jpg" or ".jpeg" or ".gif" or ".webp" or ".heic" or ".bmp" or ".tif" or ".tiff" => "\uEB9F",
            ".mp4" or ".mov" or ".mkv" or ".avi" or ".webm" or ".m4v" => "\uE714",
            ".mp3" or ".wav" or ".flac" or ".m4a" or ".aac" or ".ogg" => "\uE8D6",
            ".zip" or ".7z" or ".rar" or ".tar" or ".gz" => "\uE7B8",
            ".pdf" => "\uEA90",
            _ => "\uE8A5",
        };
    }

    /// <summary>Returns a non-existing path in <paramref name="directory"/>.</summary>
    public static string UniquePath(string directory, string baseName, string extension)
    {
        var candidate = Path.Combine(directory, baseName + extension);
        for (var index = 2; File.Exists(candidate) && index < 10_000; index++)
        {
            candidate = Path.Combine(directory, $"{baseName} ({index}){extension}");
        }

        return candidate;
    }

    private static bool StartsWith(ReadOnlySpan<byte> bytes, params byte[] prefix) =>
        bytes.Length >= prefix.Length && bytes[..prefix.Length].SequenceEqual(prefix);

    private static bool IsProbablyText(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length < 8)
        {
            return false;
        }

        var printable = 0;
        foreach (var value in bytes)
        {
            if (value == 0)
            {
                return false;
            }

            if (value is >= 0x20 and < 0x7F or 0x09 or 0x0A or 0x0D)
            {
                printable++;
            }
        }

        return printable * 100 / bytes.Length >= 95;
    }
}
