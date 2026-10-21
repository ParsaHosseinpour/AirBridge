using System;
using System.Collections.Concurrent;
using System.Diagnostics;
using System.IO;
using System.Text;

namespace AirBridge.Windows.Core;

/// <summary>
/// Small append-only diagnostic log. The UI is the primary surface, but native
/// load failures and BLE stack errors are much easier to support with a file.
/// </summary>
public static class AppLog
{
    private static readonly object Sync = new();
    private static readonly ConcurrentDictionary<string, long> LastWarning = new(StringComparer.Ordinal);
    private static readonly string LogDirectory = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
        "AirBridge",
        "logs");

    public static void Info(string message) => Write("INFO", message, null);

    public static void Warn(string message, Exception? error = null) => Write("WARN", message, error);

    public static void Error(string message, Exception? error = null) => Write("ERROR", message, error);

    /// <summary>
    /// Writes a warning at most once per <paramref name="interval"/> for the
    /// same key. The BLE coordinator retries a missing radio every few seconds;
    /// without this, one unavailable adapter would flood the log.
    /// </summary>
    public static void WarnEvery(string key, TimeSpan interval, string message, Exception? error = null)
    {
        var now = Environment.TickCount64;
        var previous = LastWarning.GetOrAdd(key, 0);

        if (previous != 0 && now - previous < (long)interval.TotalMilliseconds)
        {
            return;
        }

        if (!LastWarning.TryUpdate(key, now, previous))
        {
            return;
        }

        Warn(message, error);
    }

    public static string LogFilePath
    {
        get
        {
            var file = $"airbridge-{DateTime.Now:yyyyMMdd}.log";
            return Path.Combine(LogDirectory, file);
        }
    }

    private static void Write(string level, string message, Exception? error)
    {
        var line = new StringBuilder()
            .Append(DateTime.Now.ToString("HH:mm:ss.fff"))
            .Append(" [")
            .Append(level)
            .Append("] ")
            .Append(message);

        if (error is not null)
        {
            line.AppendLine();
            line.Append(error);
        }

        Debug.WriteLine(line.ToString());

        try
        {
            lock (Sync)
            {
                Directory.CreateDirectory(LogDirectory);
                File.AppendAllText(LogFilePath, line + Environment.NewLine);
            }
        }
        catch
        {
            // Logging must never take down a transfer or the UI.
        }
    }
}
