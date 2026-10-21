using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;
using AirBridge.Core;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Services;

public sealed class NativeCoreStatus
{
    public NativeCoreStatus(bool available, string? path, string? error)
    {
        Available = available;
        Path = path;
        Error = error;
    }

    public bool Available { get; }
    public string? Path { get; }
    public string? Error { get; }

    public string Summary => Available
        ? $"Native core ready · {Path}"
        : Error ?? "Native core unavailable";
}

/// <summary>
/// Locates and loads core_ffi.dll.
///
/// The generated bindings use <c>[DllImport("core_ffi")]</c>. During
/// development the DLL lives in target/release; after a publish it sits next to
/// AirBridge.exe. A single DllImport resolver bridges both layouts and also
/// puts the native directory on PATH so MinGW runtime DLLs are found.
/// </summary>
public static class NativeCore
{
    private const string LibraryName = "core_ffi";
    private static readonly object Sync = new();
    private static bool _resolverInstalled;
    private static NativeCoreStatus _status = new(false, null, "The native core has not been loaded yet.");

    public static NativeCoreStatus Status
    {
        get
        {
            lock (Sync)
            {
                return _status;
            }
        }
    }

    public static NativeCoreStatus Initialize()
    {
        lock (Sync)
        {
            if (_status.Available)
            {
                return _status;
            }

            try
            {
                var candidate = LocateLibrary();
                if (candidate is null)
                {
                    _status = new NativeCoreStatus(
                        false,
                        null,
                        "core_ffi.dll was not found. Build it with `pwsh scripts/build-windows.ps1` or copy it next to AirBridge.exe.");
                    return _status;
                }

                AddNativeDirectoryToPath(candidate);

                if (!_resolverInstalled)
                {
                    NativeLibrary.SetDllImportResolver(typeof(AirBridgeMethods).Assembly, Resolve);
                    _resolverInstalled = true;
                }

                // Exercise the UniFFI contract once. This catches a mismatched
                // bindings/library pair now instead of at the first transfer.
                using var probe = new PeerDiscoveryEngine(new DiscoveryConfig());
                _status = new NativeCoreStatus(true, candidate, null);
                AppLog.Info($"Native core loaded: {candidate}");
                return _status;
            }
            catch (Exception error)
            {
                _status = new NativeCoreStatus(false, null, Format.FriendlyException(error));
                AppLog.Error("Native core load failed.", error);
                return _status;
            }
        }
    }

    private static IntPtr Resolve(string libraryName, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (!string.Equals(libraryName, LibraryName, StringComparison.OrdinalIgnoreCase))
        {
            return IntPtr.Zero;
        }

        foreach (var candidate in EnumerateCandidates())
        {
            if (!File.Exists(candidate))
            {
                continue;
            }

            try
            {
                if (NativeLibrary.TryLoad(candidate, out var handle))
                {
                    return handle;
                }
            }
            catch (Exception error)
            {
                AppLog.Warn($"Native load attempt failed for {candidate}", error);
            }
        }

        return IntPtr.Zero;
    }

    private static string? LocateLibrary()
    {
        foreach (var candidate in EnumerateCandidates())
        {
            if (File.Exists(candidate))
            {
                return Path.GetFullPath(candidate);
            }
        }

        return null;
    }

    private static IEnumerable<string> EnumerateCandidates()
    {
        var environment = Environment.GetEnvironmentVariable("AIRBRIDGE_CORE_FFI");
        if (!string.IsNullOrWhiteSpace(environment))
        {
            yield return environment;
        }

        var baseDirectory = AppContext.BaseDirectory;
        yield return Path.Combine(baseDirectory, "core_ffi.dll");
        yield return Path.Combine(baseDirectory, "runtimes", "win-x64", "native", "core_ffi.dll");
        yield return Path.Combine(baseDirectory, "runtimes", "win-arm64", "native", "core_ffi.dll");
        yield return Path.Combine(baseDirectory, "runtimes", "win-x86", "native", "core_ffi.dll");

        var current = new DirectoryInfo(baseDirectory);
        for (var depth = 0; current is not null && depth < 10; depth++, current = current.Parent)
        {
            yield return Path.Combine(current.FullName, "target", "release", "core_ffi.dll");
            yield return Path.Combine(current.FullName, "target", "debug", "core_ffi.dll");
        }

        var working = new DirectoryInfo(Environment.CurrentDirectory);
        for (var depth = 0; working is not null && depth < 6; depth++, working = working.Parent)
        {
            yield return Path.Combine(working.FullName, "target", "release", "core_ffi.dll");
            yield return Path.Combine(working.FullName, "target", "debug", "core_ffi.dll");
        }
    }

    private static void AddNativeDirectoryToPath(string libraryPath)
    {
        var directory = Path.GetDirectoryName(libraryPath);
        if (string.IsNullOrEmpty(directory))
        {
            return;
        }

        var path = Environment.GetEnvironmentVariable("PATH") ?? string.Empty;
        var parts = path.Split(Path.PathSeparator, StringSplitOptions.RemoveEmptyEntries);
        foreach (var part in parts)
        {
            if (string.Equals(part.TrimEnd('\\', '/'), directory.TrimEnd('\\', '/'), StringComparison.OrdinalIgnoreCase))
            {
                return;
            }
        }

        Environment.SetEnvironmentVariable("PATH", directory + Path.PathSeparator + path);
    }
}
