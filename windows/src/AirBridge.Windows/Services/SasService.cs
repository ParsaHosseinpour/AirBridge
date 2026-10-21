using System;
using System.Globalization;
using System.Security.Cryptography;
using System.Threading.Tasks;

namespace AirBridge.Windows.Services;

/// <summary>A short authentication string the user compares out of band.</summary>
public sealed class SasChallenge
{
    public SasChallenge(string peerIdHex, string code, DateTimeOffset createdAt)
    {
        PeerIdHex = peerIdHex;
        Code = code;
        CreatedAt = createdAt;
    }

    public string PeerIdHex { get; }
    public string Code { get; }
    public DateTimeOffset CreatedAt { get; }
    public bool IsExpired => DateTimeOffset.UtcNow - CreatedAt > TimeSpan.FromMinutes(2);
}

/// <summary>
/// Boundary between the UI and the cryptographic pairing handshake.
///
/// <c>libcore_ffi</c> currently exports discovery and one-file transfer
/// sessions, but not the crypto crate's SAS handshake. The Windows app
/// therefore talks to this seam, exactly as the Android app does. The preview
/// implementation must not be presented as proof of identity, and it never
/// persists trust.
/// </summary>
public interface ISasGateway
{
    /// <summary>Whether this gateway is a local demo rather than real verification.</summary>
    bool IsPreview { get; }

    Task<SasChallenge> ChallengeAsync(string peerIdHex);

    Task<bool> ApproveAsync(SasChallenge challenge);
}

/// <summary>
/// Development stand-in used until the Rust SAS handshake is exported across
/// the FFI. It produces a local six-digit code, simulates the round trip, and
/// accepts any unexpired challenge. It deliberately does not authenticate the
/// remote device.
/// </summary>
public sealed class LocalPreviewSasGateway : ISasGateway
{
    private static readonly TimeSpan ChallengeLifetime = TimeSpan.FromMinutes(2);

    public bool IsPreview => true;

    public async Task<SasChallenge> ChallengeAsync(string peerIdHex)
    {
        await Task.Delay(220).ConfigureAwait(false);
        var code = RandomNumberGenerator.GetInt32(0, 1_000_000)
            .ToString("D6", CultureInfo.InvariantCulture);
        return new SasChallenge(peerIdHex, code, DateTimeOffset.UtcNow);
    }

    public async Task<bool> ApproveAsync(SasChallenge challenge)
    {
        if (challenge is null)
        {
            throw new ArgumentNullException(nameof(challenge));
        }

        await Task.Delay(260).ConfigureAwait(false);
        return !challenge.IsExpired && DateTimeOffset.UtcNow - challenge.CreatedAt < ChallengeLifetime;
    }
}
