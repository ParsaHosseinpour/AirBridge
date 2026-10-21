using System;
using System.Linq;
using AirBridge.Windows.Services;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AirBridge.Windows.Dialogs;

/// <summary>
/// Compare-the-code pairing prompt backed by the P-256 handshake. The code is
/// the SAS from the encrypted exchange; the fingerprint is the durable identity
/// that is stored when the user approves.
/// </summary>
public sealed partial class SasVerificationDialog : ContentDialog
{
    private readonly SasPrompt _prompt;
    private readonly DispatcherQueueTimer _timer;

    public SasVerificationDialog(SasPrompt prompt)
    {
        ArgumentNullException.ThrowIfNull(prompt);

        InitializeComponent();

        _prompt = prompt;
        Title = prompt.IsIncoming ? "Verify incoming device" : "Verify device";
        PeerNameText.Text = prompt.PeerName;
        PeerDetailText.Text = prompt.PeerDetail;
        PeerGlyph.Glyph = prompt.Glyph;
        CodeText.Text = FormatCode(prompt.Sas);
        FingerprintText.Text = prompt.GroupedFingerprint;

        if (prompt.IsIncoming)
        {
            InstructionText.Text =
                "A device is asking to connect. Compare this code with the screen of the person sending the file.";
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
        var remaining = _prompt.ExpiresAt - DateTimeOffset.UtcNow;
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
        var digits = new string((code ?? string.Empty).Where(char.IsLetterOrDigit).ToArray());
        if (digits.Length == 6)
        {
            return $"{digits[..3]} {digits[3..]}";
        }

        return string.Join(' ', digits.Chunk(3).Select(chunk => new string(chunk)));
    }
}
