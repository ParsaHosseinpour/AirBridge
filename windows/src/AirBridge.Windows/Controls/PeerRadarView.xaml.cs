using System;
using System.Collections;
using System.Collections.Generic;
using System.Collections.Specialized;
using System.Linq;
using AirBridge.Windows.Core;
using AirBridge.Windows.Models;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Animation;
using Microsoft.UI.Xaml.Shapes;
using Windows.ApplicationModel.DataTransfer;
using Windows.Foundation;
using Windows.Storage;
using Windows.UI;

namespace AirBridge.Windows.Controls;

public sealed class PeerInvokedEventArgs : EventArgs
{
    public PeerInvokedEventArgs(PeerDevice peer) => Peer = peer;

    public PeerDevice Peer { get; }
}

/// <summary>
/// Files dropped on the radar. <see cref="Files"/> covers StorageFile-backed
/// drag sources (including virtual providers); <see cref="Paths"/> covers plain
/// text drops from tools that only publish a path.
/// </summary>
public sealed class FilesDroppedEventArgs : EventArgs
{
    public FilesDroppedEventArgs(IReadOnlyList<StorageFile> files, IReadOnlyList<string> paths)
    {
        Files = files;
        Paths = paths;
    }

    public IReadOnlyList<StorageFile> Files { get; }
    public IReadOnlyList<string> Paths { get; }
}

/// <summary>
/// The interactive radar stage: an orbit of nearby devices around the local
/// drop target. This control only renders and raises intents; selection and
/// transfer state live in the view model.
/// </summary>
public sealed partial class PeerRadarView : UserControl
{
    public static readonly DependencyProperty ItemsSourceProperty = DependencyProperty.Register(
        nameof(ItemsSource),
        typeof(object),
        typeof(PeerRadarView),
        new PropertyMetadata(null, OnItemsSourceChanged));

    public static readonly DependencyProperty InitialsProperty = DependencyProperty.Register(
        nameof(Initials),
        typeof(string),
        typeof(PeerRadarView),
        new PropertyMetadata("AB"));

    public static readonly DependencyProperty DeviceNameProperty = DependencyProperty.Register(
        nameof(DeviceName),
        typeof(string),
        typeof(PeerRadarView),
        new PropertyMetadata("My PC"));

    public static readonly DependencyProperty StatusTextProperty = DependencyProperty.Register(
        nameof(StatusText),
        typeof(string),
        typeof(PeerRadarView),
        new PropertyMetadata("Click or drop files"));

    public static readonly DependencyProperty DropActiveProperty = DependencyProperty.Register(
        nameof(DropActive),
        typeof(bool),
        typeof(PeerRadarView),
        new PropertyMetadata(false, OnDropActiveChanged));

    public static readonly DependencyProperty StagedCountProperty = DependencyProperty.Register(
        nameof(StagedCount),
        typeof(int),
        typeof(PeerRadarView),
        new PropertyMetadata(0, OnStagedCountChanged));

    public static readonly DependencyProperty IsScanningProperty = DependencyProperty.Register(
        nameof(IsScanning),
        typeof(bool),
        typeof(PeerRadarView),
        new PropertyMetadata(false, OnIsScanningChanged));

    private Storyboard? _sweepStoryboard;
    private Storyboard? _dropStoryboard;
    private INotifyCollectionChanged? _observedCollection;

    public PeerRadarView()
    {
        InitializeComponent();
        Loaded += OnLoaded;
    }

    public event EventHandler<PeerInvokedEventArgs>? PeerInvoked;
    public event EventHandler? BrowseRequested;
    public event EventHandler<FilesDroppedEventArgs>? FilesDropped;

    public object? ItemsSource
    {
        get => GetValue(ItemsSourceProperty);
        set => SetValue(ItemsSourceProperty, value);
    }

    public string Initials
    {
        get => (string)GetValue(InitialsProperty);
        set => SetValue(InitialsProperty, value);
    }

    public string DeviceName
    {
        get => (string)GetValue(DeviceNameProperty);
        set => SetValue(DeviceNameProperty, value);
    }

    public string StatusText
    {
        get => (string)GetValue(StatusTextProperty);
        set => SetValue(StatusTextProperty, value);
    }

    public bool DropActive
    {
        get => (bool)GetValue(DropActiveProperty);
        set => SetValue(DropActiveProperty, value);
    }

    public int StagedCount
    {
        get => (int)GetValue(StagedCountProperty);
        set => SetValue(StagedCountProperty, value);
    }

    public bool IsScanning
    {
        get => (bool)GetValue(IsScanningProperty);
        set => SetValue(IsScanningProperty, value);
    }

    private void OnLoaded(object sender, RoutedEventArgs args)
    {
        BuildTicks();
        EnsureAnimations();
        UpdateSweep();
        UpdateDropVisual();
        UpdateEmptyState();
    }

    private static void OnItemsSourceChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        if (sender is PeerRadarView view)
        {
            view.ObserveCollection(args.OldValue, args.NewValue);
        }
    }

    private void ObserveCollection(object? oldValue, object? newValue)
    {
        if (_observedCollection is not null)
        {
            _observedCollection.CollectionChanged -= OnSourceCollectionChanged;
            _observedCollection = null;
        }

        if (newValue is INotifyCollectionChanged observable)
        {
            _observedCollection = observable;
            observable.CollectionChanged += OnSourceCollectionChanged;
        }

        UpdateEmptyState();
    }

    private void OnSourceCollectionChanged(object? sender, NotifyCollectionChangedEventArgs args) => UpdateEmptyState();

    private void UpdateEmptyState()
    {
        if (EmptyHint is null)
        {
            return;
        }

        var count = ItemsSource switch
        {
            null => 0,
            ICollection collection => collection.Count,
            IEnumerable items => items.Cast<object>().Count(),
            _ => 0,
        };

        EmptyHint.Visibility = count == 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private static void OnStagedCountChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        if (sender is not PeerRadarView view || view.StagedBadge is null)
        {
            return;
        }

        var count = Math.Max(0, (int)args.NewValue);
        view.StagedBadgeText.Text = count.ToString();
        view.StagedBadge.Visibility = count > 0 ? Visibility.Visible : Visibility.Collapsed;
    }

    private static void OnIsScanningChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        if (sender is PeerRadarView { IsLoaded: true } view)
        {
            view.UpdateSweep();
        }
    }

    private static void OnDropActiveChanged(DependencyObject sender, DependencyPropertyChangedEventArgs args)
    {
        if (sender is PeerRadarView { IsLoaded: true } view)
        {
            view.UpdateDropVisual();
        }
    }

    private void EnsureAnimations()
    {
        if (_sweepStoryboard is null)
        {
            var rotation = new DoubleAnimation
            {
                From = 0,
                To = 360,
                Duration = new Duration(TimeSpan.FromSeconds(9)),
                RepeatBehavior = RepeatBehavior.Forever,
            };

            Storyboard.SetTarget(rotation, SweepRotate);
            Storyboard.SetTargetProperty(rotation, "Angle");

            _sweepStoryboard = new Storyboard();
            _sweepStoryboard.Children.Add(rotation);
        }

        if (_dropStoryboard is null)
        {
            var pulse = new DoubleAnimation
            {
                From = 0.18,
                To = 1,
                Duration = new Duration(TimeSpan.FromSeconds(0.9)),
                AutoReverse = true,
                RepeatBehavior = RepeatBehavior.Forever,
                EnableDependentAnimation = true,
            };

            Storyboard.SetTarget(pulse, DropRing);
            Storyboard.SetTargetProperty(pulse, "Opacity");

            _dropStoryboard = new Storyboard();
            _dropStoryboard.Children.Add(pulse);
        }
    }

    private void UpdateSweep()
    {
        if (!IsLoaded)
        {
            return;
        }

        EnsureAnimations();

        if (IsScanning)
        {
            SweepPath.Opacity = 0.55;
            _sweepStoryboard?.Begin();
        }
        else
        {
            _sweepStoryboard?.Stop();
            SweepPath.Opacity = 0.12;
        }
    }

    private void UpdateDropVisual()
    {
        if (!IsLoaded)
        {
            return;
        }

        EnsureAnimations();

        if (DropActive)
        {
            _dropStoryboard?.Begin();
        }
        else
        {
            _dropStoryboard?.Stop();
            if (DropRing is not null)
            {
                DropRing.Opacity = 0;
            }
        }
    }

    /// <summary>Draws the faint instrument ticks around the outer ring.</summary>
    private void BuildTicks()
    {
        TicksCanvas.Children.Clear();

        const double centerX = 280;
        const double centerY = 260;
        const double outerRadius = 210;

        for (var index = 0; index < 60; index++)
        {
            var major = index % 5 == 0;
            var angle = (index * 6d) * Math.PI / 180d;
            var innerRadius = major ? 199 : 205;

            var tick = new Line
            {
                X1 = centerX + (Math.Cos(angle) * innerRadius),
                Y1 = centerY + (Math.Sin(angle) * innerRadius),
                X2 = centerX + (Math.Cos(angle) * outerRadius),
                Y2 = centerY + (Math.Sin(angle) * outerRadius),
                Stroke = new SolidColorBrush(Color.FromArgb(major ? (byte)0x3A : (byte)0x1A, 255, 255, 255)),
                StrokeThickness = major ? 1.3 : 1,
            };

            TicksCanvas.Children.Add(tick);
        }
    }

    private void OnPeerClick(object sender, RoutedEventArgs args)
    {
        if (sender is FrameworkElement { DataContext: PeerDevice peer })
        {
            PeerInvoked?.Invoke(this, new PeerInvokedEventArgs(peer));
        }
    }

    private void OnBrowseClick(object sender, RoutedEventArgs args) =>
        BrowseRequested?.Invoke(this, EventArgs.Empty);

    private void OnDragOver(object sender, DragEventArgs args)
    {
        if (!args.DataView.Contains(StandardDataFormats.StorageItems))
        {
            args.AcceptedOperation = DataPackageOperation.None;
            DropActive = false;
            return;
        }

        args.AcceptedOperation = DataPackageOperation.Copy;
        args.DragUIOverride.Caption = "Send with AirBridge";
        args.DragUIOverride.IsCaptionVisible = true;
        args.DragUIOverride.IsGlyphVisible = true;
        DropActive = true;
    }

    private void OnDragLeave(object sender, DragEventArgs args) => DropActive = false;

    private async void OnDrop(object sender, DragEventArgs args)
    {
        DropActive = false;

        if (!args.DataView.Contains(StandardDataFormats.StorageItems))
        {
            return;
        }

        var deferral = args.GetDeferral();

        try
        {
            var items = await args.DataView.GetStorageItemsAsync();
            var files = new List<StorageFile>();
            var paths = new List<string>();

            foreach (var item in items)
            {
                if (item is StorageFile file)
                {
                    files.Add(file);
                    if (!string.IsNullOrWhiteSpace(file.Path))
                    {
                        paths.Add(file.Path);
                    }
                }
            }

            if (files.Count == 0 && args.DataView.Contains(StandardDataFormats.Text))
            {
                var text = await args.DataView.GetTextAsync();
                foreach (var line in text.Split(new[] { '\r', '\n' }, StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
                {
                    if (!string.IsNullOrWhiteSpace(line))
                    {
                        paths.Add(line);
                    }
                }
            }

            if (files.Count > 0)
            {
                paths.Clear();
            }

            if (files.Count > 0 || paths.Count > 0)
            {
                FilesDropped?.Invoke(this, new FilesDroppedEventArgs(files, paths));
            }
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not read the dropped items.", error);
        }
        finally
        {
            deferral.Complete();
        }
    }
}
