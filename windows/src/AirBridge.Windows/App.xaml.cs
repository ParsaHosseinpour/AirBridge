using System;
using System.Diagnostics;
using AirBridge.Windows.Core;
using AirBridge.Windows.Services;
using Microsoft.UI.Xaml;

namespace AirBridge.Windows;

/// <summary>
/// Application entry point. The native resolver is installed before any
/// AirBridge.Core type can reach a P/Invoke, and the main window owns the
/// lifetime of discovery, the BLE radio and transfer sessions.
/// </summary>
public partial class App : Application
{
    private MainWindow? _window;

    public App()
    {
        InitializeComponent();
        UnhandledException += OnUnhandledException;
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        // This must happen before the first generated binding call: .NET only
        // permits a DllImport resolver to be installed once per assembly.
        NativeCore.Initialize();

        _window = new MainWindow();
        _window.Activate();
    }

    private void OnUnhandledException(object sender, Microsoft.UI.Xaml.UnhandledExceptionEventArgs e)
    {
        AppLog.Error("Unhandled XAML exception", e.Exception);
        Debug.WriteLine(e.Exception);
        e.Handled = true;
    }
}
