using System;
using System.Linq;

namespace AirBridge.Windows.Services;

/// <summary>
/// A short authentication string produced by the P-256 handshake, together
/// with the peer key it authenticates. The user compares the code out of band
/// and the fingerprint is what is remembered as trusted.
/// </summary>
public sealed class SasPrompt
{
    public SasPrompt(
        string sas,
        string peerFingerprint,
        string peerName,
        string peerDetail,
        bool isIncoming,
        string glyph = "\uE7BA",
        TimeSpan? lifetime = null)
    {
        Sas = string.IsNullOrWhiteSpace(sas) ? string.Empty : sas.Trim();
        PeerFingerprint = string.IsNullOrWhiteSpace(peerFingerprint)
            ? string.Empty
            : peerFingerprint.Trim().ToLowerInvariant();
        PeerName = string.IsNullOrWhiteSpace(peerName) ? "Nearby device" : peerName.Trim();
        PeerDetail = peerDetail?.Trim() ?? string.Empty;
        IsIncoming = isIncoming;
        Glyph = string.IsNullOrWhiteSpace(glyph) ? "\uE7BA" : glyph;
        CreatedAt = DateTimeOffset.UtcNow;
        Lifetime = lifetime ?? TimeSpan.FromMinutes(2);
    }

    /// <summary>Six digits from the handshake transcript.</summary>
    public string Sas { get; }

    /// <summary>SHA-256 fingerprint of the peer's long-lived identity key.</summary>
    public string PeerFingerprint { get; }

    public string PeerName { get; }

    public string PeerDetail { get; }

    public bool IsIncoming { get; }

    /// <summary>Segoe MDL2 glyph used by the dialog avatar.</summary>
    public string Glyph { get; }

    public DateTimeOffset CreatedAt { get; }

    public TimeSpan Lifetime { get; }

    public DateTimeOffset ExpiresAt => CreatedAt + Lifetime;

    public string ShortFingerprint =>
        string.IsNullOrWhiteSpace(PeerFingerprint)
            ? "unknown key"
            : PeerFingerprint.Length <= 16
                ? PeerFingerprint
                : $"{PeerFingerprint[..8]}…{PeerFingerprint[^8..]}";

    public string GroupedFingerprint
    {
        get
        {
            if (string.IsNullOrWhiteSpace(PeerFingerprint))
            {
                return string.Empty;
            }

            var chunks = PeerFingerprint
                .ToLowerInvariant()
                .Chunk(4)
                .Select(chunk => new string(chunk));
            return string.Join(' ', chunks);
        }
    }
}
