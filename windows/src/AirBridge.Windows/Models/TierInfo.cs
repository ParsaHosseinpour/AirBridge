using AirBridge.Core;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Models;

/// <summary>
/// One discovery tier's health, shaped for the status strip under the radar.
/// </summary>
public sealed class TierInfo : ObservableObject
{
    private bool _isActive;
    private string _stateText = "Waiting";
    private string _stateColor = "#FF9DB2C4";
    private string _detail = string.Empty;

    public TierInfo(DiscoveryTier tier, string name, string glyph)
    {
        Tier = tier;
        Name = name;
        Glyph = glyph;
    }

    public DiscoveryTier Tier { get; }
    public string Name { get; }
    public string Glyph { get; }

    public bool IsActive
    {
        get => _isActive;
        private set => SetProperty(ref _isActive, value);
    }

    public string StateText
    {
        get => _stateText;
        private set => SetProperty(ref _stateText, value);
    }

    public string StateColor
    {
        get => _stateColor;
        private set => SetProperty(ref _stateColor, value);
    }

    public string Detail
    {
        get => _detail;
        internal set
        {
            if (SetProperty(ref _detail, value))
            {
                OnPropertyChanged(nameof(HasDetail));
                OnPropertyChanged(nameof(Tooltip));
            }
        }
    }

    public bool HasDetail => !string.IsNullOrWhiteSpace(Detail);

    public string Tooltip => string.IsNullOrWhiteSpace(Detail) ? $"{Name}: {StateText}" : $"{Name}: {StateText} \u2014 {Detail}";

    public void Reset()
    {
        IsActive = false;
        StateText = "Waiting";
        StateColor = "#FF9DB2C4";
        Detail = string.Empty;
    }

    public void Apply(TierState state)
    {
        switch (state)
        {
            case TierState.Starting:
                IsActive = false;
                StateText = "Starting";
                StateColor = "#FF9FE7FF";
                Detail = string.Empty;
                break;
            case TierState.Active:
                IsActive = true;
                StateText = "Active";
                StateColor = "#FF6EE7C8";
                Detail = string.Empty;
                break;
            case TierState.Degraded degraded:
                IsActive = false;
                StateText = "Limited";
                StateColor = "#FFFFCF70";
                Detail = degraded.Reason;
                break;
            case TierState.Failed failed:
                IsActive = false;
                StateText = "Unavailable";
                StateColor = "#FFFF6B7A";
                Detail = failed.Message;
                break;
            case TierState.Stopped:
                IsActive = false;
                StateText = "Stopped";
                StateColor = "#FF9DB2C4";
                Detail = string.Empty;
                break;
        }
    }
}
