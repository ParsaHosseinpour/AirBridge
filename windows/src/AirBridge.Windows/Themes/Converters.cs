using System;
using System.Collections.Generic;
using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Data;
using Microsoft.UI.Xaml.Media;
using Windows.UI;

namespace AirBridge.Windows.Themes;

/// <summary>True becomes Visible; everything else is Collapsed.</summary>
public sealed class BoolToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? Visibility.Visible : Visibility.Collapsed;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        value is Visibility.Visible;
}

public sealed class InverseBoolToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is true ? Visibility.Collapsed : Visibility.Visible;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        value is not Visibility.Visible;
}

/// <summary>Null becomes Collapsed; a value becomes Visible.</summary>
public sealed class NullToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is null ? Visibility.Collapsed : Visibility.Visible;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

public sealed class InverseNullToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is null ? Visibility.Visible : Visibility.Collapsed;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

/// <summary>Zero becomes Collapsed; any non-zero number becomes Visible.</summary>
public sealed class CountToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language)
    {
        var count = value switch
        {
            int i => i,
            long l => l,
            double d => (int)d,
            _ => 0,
        };
        return count == 0 ? Visibility.Collapsed : Visibility.Visible;
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

/// <summary>Collapses when the string is null or whitespace.</summary>
public sealed class StringToVisibilityConverter : IValueConverter
{
    public object Convert(object value, Type targetType, object parameter, string language) =>
        value is string text && !string.IsNullOrWhiteSpace(text)
            ? Visibility.Visible
            : Visibility.Collapsed;

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();
}

/// <summary>Turns an <c>#AARRGGBB</c> string into a cached solid brush.</summary>
public sealed class HexBrushConverter : IValueConverter
{
    private readonly Dictionary<string, SolidColorBrush> _cache = new(StringComparer.OrdinalIgnoreCase);

    public object Convert(object value, Type targetType, object parameter, string language)
    {
        var text = value as string;
        if (string.IsNullOrWhiteSpace(text))
        {
            text = "#FF9DB2C4";
        }

        lock (_cache)
        {
            if (_cache.TryGetValue(text, out var cached))
            {
                return cached;
            }

            var brush = new SolidColorBrush(Parse(text));
            _cache[text] = brush;
            return brush;
        }
    }

    public object ConvertBack(object value, Type targetType, object parameter, string language) =>
        throw new NotSupportedException();

    private static Color Parse(string text)
    {
        var value = text.TrimStart('#');

        try
        {
            return value.Length switch
            {
                8 => Color.FromArgb(
                    System.Convert.ToByte(value[..2], 16),
                    System.Convert.ToByte(value.Substring(2, 2), 16),
                    System.Convert.ToByte(value.Substring(4, 2), 16),
                    System.Convert.ToByte(value.Substring(6, 2), 16)),
                6 => Color.FromArgb(
                    0xFF,
                    System.Convert.ToByte(value[..2], 16),
                    System.Convert.ToByte(value.Substring(2, 2), 16),
                    System.Convert.ToByte(value.Substring(4, 2), 16)),
                _ => Colors.Cyan,
            };
        }
        catch
        {
            return Colors.Cyan;
        }
    }
}
