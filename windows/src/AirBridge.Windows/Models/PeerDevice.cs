using System;
using System.Collections.Generic;
using System.Linq;
using AirBridge.Core;
using AirBridge.Windows.Core;
using Microsoft.UI.Xaml.Media;
using Windows.UI;
using Point = Windows.Foundation.Point;

namespace AirBridge.Windows.Models;

/// <summary>A discovered peer, shaped for the radar and the device grid.</summary>
public sealed class PeerDevice : ObservableObject
{
    private static readonly (Color Start, Color End)[] Palettes =
    {
        (Color.FromArgb(255, 52, 145, 214), Color.FromArgb(255, 13, 48, 82)),
        (Color.FromArgb(255, 58, 194, 170), Color.FromArgb(255, 10, 61, 64)),
        (Color.FromArgb(255, 126, 107, 232), Color.FromArgb(255, 37, 27, 91)),
        (Color.FromArgb(255, 228, 149, 83), Color.FromArgb(255, 87, 41, 22)),
        (Color.FromArgb(255, 220, 101, 137), Color.FromArgb(255, 86, 25, 55)),
        (Color.FromArgb(255, 87, 188, 232), Color.FromArgb(255, 20, 61, 91)),
    };

    private string _name;
    private string _addressText = "Discovering address…";
    private bool _isTrusted;
    private bool _isSelected;
    private string[] _addresses = Array.Empty<string>();
    private DeviceKind _kind;
    private LinearGradientBrush? _avatarBrush;

    public PeerDevice(PeerInfo peer, bool isTrusted)
    {
        IdHex = peer.IdHex;
        _name = Format.DisplayName(peer.Name, peer.IdHex);
        _kind = DeviceKinds.Guess(peer.Name);
        _addresses = peer.Addresses ?? Array.Empty<string>();
        _addressText = BuildAddressText(_addresses);
        _isTrusted = isTrusted;
    }

    public string IdHex { get; }

    public string Name
    {
        get => _name;
        private set
        {
            if (SetProperty(ref _name, value))
            {
                OnPropertiesChanged(nameof(Initials), nameof(AutomationName));
            }
        }
    }

    public string[] Addresses
    {
        get => _addresses;
        private set
        {
            if (SetProperty(ref _addresses, value))
            {
                OnPropertiesChanged(nameof(AddressText), nameof(PrimaryAddress), nameof(HasAddress), nameof(IsReachable));
            }
        }
    }

    public string AddressText
    {
        get => _addressText;
        private set => SetProperty(ref _addressText, value);
    }

    public bool IsTrusted
    {
        get => _isTrusted;
        set
        {
            if (SetProperty(ref _isTrusted, value))
            {
                OnPropertiesChanged(nameof(TrustLabel), nameof(AutomationName));
            }
        }
    }

    /// <summary>True while this peer is the send target selected in the UI.</summary>
    public bool IsSelected
    {
        get => _isSelected;
        set => SetProperty(ref _isSelected, value);
    }

    public DeviceKind Kind
    {
        get => _kind;
        private set
        {
            if (SetProperty(ref _kind, value))
            {
                OnPropertiesChanged(nameof(Glyph), nameof(DeviceLabel));
            }
        }
    }

    public string Glyph => DeviceKinds.Glyph(Kind);
    public string DeviceLabel => DeviceKinds.Label(Kind);
    public string Initials => Format.Initials(Name);
    public string ShortId => Format.ShortId(IdHex);
    public string TrustLabel => IsTrusted ? "Verified device" : "Not verified";
    public bool HasAddress => Addresses.Length > 0;
    public bool IsReachable => HasAddress;
    public string PrimaryAddress => Addresses.FirstOrDefault() ?? string.Empty;
    public string AutomationName => $"{Name}, {DeviceLabel}, {(IsTrusted ? "verified" : "not verified")}";

    public string StatusLine => HasAddress
        ? $"{DeviceLabel} · {Addresses[0]}"
        : $"{DeviceLabel} · Bluetooth only";

    public LinearGradientBrush AvatarBrush
    {
        get
        {
            if (_avatarBrush is not null)
            {
                return _avatarBrush;
            }

            var index = 0;
            foreach (var character in IdHex)
            {
                index = (index * 31 + character) & 0x7FFFFFFF;
            }

            var palette = Palettes[index % Palettes.Length];
            var brush = new LinearGradientBrush
            {
                StartPoint = new Point(0, 0),
                EndPoint = new Point(1, 1),
            };
            brush.GradientStops.Add(new GradientStop { Color = palette.Start, Offset = 0 });
            brush.GradientStops.Add(new GradientStop { Color = palette.End, Offset = 1 });
            _avatarBrush = brush;
            return brush;
        }
    }

    /// <summary>Merges an updated snapshot without replacing the object identity.</summary>
    public void Update(PeerInfo peer, bool isTrusted)
    {
        var displayName = Format.DisplayName(peer.Name, peer.IdHex);
        if (!string.Equals(Name, displayName, StringComparison.Ordinal))
        {
            Name = displayName;
        }

        Kind = DeviceKinds.Guess(peer.Name);
        IsTrusted = isTrusted;

        var addresses = peer.Addresses ?? Array.Empty<string>();
        if (!addresses.SequenceEqual(Addresses, StringComparer.Ordinal))
        {
            Addresses = addresses;
            AddressText = BuildAddressText(addresses);
            OnPropertyChanged(nameof(StatusLine));
        }
    }

    private static string BuildAddressText(IReadOnlyList<string> addresses) =>
        addresses.Count == 0 ? "Looking for an IP address…" : addresses[0];
}
