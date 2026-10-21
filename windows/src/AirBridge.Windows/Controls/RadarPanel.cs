using System;
using Windows.Foundation;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AirBridge.Windows.Controls;

/// <summary>
/// Arranges children evenly around an ellipse: the radar orbit. The panel
/// measures each child at its natural size and arranges them so the centre of
/// the child sits on the ring; items keep their bubble identity and are only
/// repositioned when the peer set changes.
/// </summary>
public sealed class RadarPanel : Panel
{
    /// <summary>Horizontal radius as a fraction of the panel width.</summary>
    public static readonly DependencyProperty RingXProperty = DependencyProperty.Register(
        nameof(RingX),
        typeof(double),
        typeof(RadarPanel),
        new PropertyMetadata(0.37, OnLayoutPropertyChanged));

    /// <summary>Vertical radius as a fraction of the panel height.</summary>
    public static readonly DependencyProperty RingYProperty = DependencyProperty.Register(
        nameof(RingY),
        typeof(double),
        typeof(RadarPanel),
        new PropertyMetadata(0.33, OnLayoutPropertyChanged));

    /// <summary>Angle offset in degrees; -90 puts the first child at the top.</summary>
    public static readonly DependencyProperty StartAngleProperty = DependencyProperty.Register(
        nameof(StartAngle),
        typeof(double),
        typeof(RadarPanel),
        new PropertyMetadata(-90d, OnLayoutPropertyChanged));

    public double RingX
    {
        get => (double)GetValue(RingXProperty);
        set => SetValue(RingXProperty, value);
    }

    public double RingY
    {
        get => (double)GetValue(RingYProperty);
        set => SetValue(RingYProperty, value);
    }

    public double StartAngle
    {
        get => (double)GetValue(StartAngleProperty);
        set => SetValue(StartAngleProperty, value);
    }

    protected override Size MeasureOverride(Size availableSize)
    {
        var width = double.IsInfinity(availableSize.Width) ? 520 : availableSize.Width;
        var height = double.IsInfinity(availableSize.Height) ? 520 : availableSize.Height;

        foreach (var child in Children)
        {
            child.Measure(new Size(double.PositiveInfinity, double.PositiveInfinity));
        }

        return new Size(width, height);
    }

    protected override Size ArrangeOverride(Size finalSize)
    {
        var count = Children.Count;
        if (count == 0)
        {
            return finalSize;
        }

        var centerX = finalSize.Width / 2;
        var centerY = finalSize.Height / 2;
        var radiusX = finalSize.Width * RingX;
        var radiusY = finalSize.Height * RingY;
        var step = 360d / count;

        for (var index = 0; index < count; index++)
        {
            var child = Children[index];
            var desired = child.DesiredSize;
            var jitter = Jitter(child);
            var angle = (StartAngle + (step * index) + jitter) * Math.PI / 180d;

            var x = centerX + (Math.Cos(angle) * radiusX) - (desired.Width / 2);
            var y = centerY + (Math.Sin(angle) * radiusY) - (desired.Height / 2);

            child.Arrange(new Rect(x, y, desired.Width, desired.Height));
        }

        return finalSize;
    }

    /// <summary>A small deterministic offset so the orbit feels organic, not gridded.</summary>
    private static double Jitter(UIElement child)
    {
        var hash = child.GetHashCode() & 0x7FFFFFFF;
        return ((hash % 13) - 6) * 1.6;
    }

    private static void OnLayoutPropertyChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args) =>
        ((RadarPanel)sender).InvalidateArrange();
}
