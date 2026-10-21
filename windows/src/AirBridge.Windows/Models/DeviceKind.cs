namespace AirBridge.Windows.Models;

public enum DeviceKind
{
    Unknown,
    Phone,
    Tablet,
    Laptop,
    Desktop,
}

public static class DeviceKinds
{
    /// <summary>
    /// The discovery protocol carries a name, not a hardware class, so the
    /// icon is a friendly best-effort guess based on common device names.
    /// </summary>
    public static DeviceKind Guess(string? name)
    {
        var value = name?.ToLowerInvariant() ?? string.Empty;
        if (value.Contains("iphone") || value.Contains("pixel") || value.Contains("galaxy") ||
            value.Contains("phone") || value.Contains("oneplus") || value.Contains("xiaomi"))
        {
            return DeviceKind.Phone;
        }

        if (value.Contains("ipad") || value.Contains("tablet") || value.Contains("tab "))
        {
            return DeviceKind.Tablet;
        }

        if (value.Contains("macbook") || value.Contains("laptop") || value.Contains("notebook") ||
            value.Contains("surface") || value.Contains("book") || value.Contains("thinkpad"))
        {
            return DeviceKind.Laptop;
        }

        if (value.Contains("desktop") || value.Contains("workstation") || value.Contains("pc") ||
            value.Contains("imac") || value.Contains("windows"))
        {
            return DeviceKind.Desktop;
        }

        return DeviceKind.Unknown;
    }

    public static string Glyph(DeviceKind kind) => kind switch
    {
        DeviceKind.Phone => "\uE8EA",
        DeviceKind.Tablet => "\uE70A",
        DeviceKind.Laptop => "\uE7F8",
        DeviceKind.Desktop => "\uE7F4",
        _ => "\uE702",
    };

    public static string Label(DeviceKind kind) => kind switch
    {
        DeviceKind.Phone => "Phone",
        DeviceKind.Tablet => "Tablet",
        DeviceKind.Laptop => "Laptop",
        DeviceKind.Desktop => "Computer",
        _ => "Device",
    };
}
