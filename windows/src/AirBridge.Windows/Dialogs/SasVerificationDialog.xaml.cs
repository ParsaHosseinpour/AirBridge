using System;
using AirBridge.Windows.Models;
using AirBridge.Windows.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AirBridge.Windows.Dialogs;

/// <summary>
/// Compare-the-code pairing prompt. When the gateway is a preview, the dialog
/// says so in place and explains that the code is not cryptographic proof.
/// </summary>
public sealed partial class SasVerificationDialog : ContentDialog
{
    private static readonly TimeSpan ChallengeLifetime = TimeSpan.FromMinutes(2);

    private readonly SasChallenge _challenge;
    private readonly DispatcherQueueTimer _timer;

    public SasVerificationDialog(PeerDevice peer, SasChallenge challenge, bool isPreview)
    {
        ArgumentNullException.ThrowIfNull(peer);
        ArgumentNullException.ThrowIfNull(challenge);

        InitializeComponent();

        _challenge = challenge;
        PeerNameText.Text = peer.Name;
        PeerDetailText.Text = $"{peer.DeviceLabel} \u00B7 {peer.AddressText}";
        PeerAvatar.Fill = peer.AvatarBrush;
        PeerGlyph.Glyph = peer.Glyph;
        CodeText.Text = FormatCode(challenge.Code);

        PreviewPanel.Visibility = isPreview ? Visibility.Visible : Visibility.Collapsed;

        if (isPreview)
        {
            PrimaryButtonText = "Approve preview";
            InstructionText.Text =
                "This build cannot exchange a PIN with the other device, so the two screens will not show the same code.";
            FooterText.Text =
                "Preview pairing does not verify the other device. Approve only when you trust the local network.";
        }

        _timer = DispatcherQueue.CreateTimer();
        _timer.Interval = TimeSpan.FromSeconds(1);
        _timer.Tick += (_, _) => UpdateExpiry();
        _timer.Start();

        Closed += (_, _) => _timer.Stop();
        UpdateExpiry();
    }

    private void UpdateExpiry()
    {
        var remaining = ChallengeLifetime - (DateTimeOffset.UtcNow - _challenge.CreatedAt);
        if (remaining <= TimeSpan.Zero)
        {
            ExpiryText.Text = "This code has expired.";
            ExpiryText.Foreground = (Microsoft.UI.Xaml.Media.Brush)Application.Current.Resources["AirBridgeDangerBrush"];
            IsPrimaryButtonEnabled = false;
            _timer.Stop();
            return;
        }

        ExpiryText.Text = $"Expires in {Math.Floor(remaining.TotalMinutes):0}:{remaining.Seconds:00}";
        IsPrimaryButtonEnabled = true;
    }

    private static string FormatCode(string code)
    {
        code = code?.Trim() ?? string.Empty;
        return code.Length == 6 ? $"{code[..3]} {code[3..]}" : code;
    }
}
