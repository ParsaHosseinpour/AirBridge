using System;
using System.Diagnostics;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
using AirBridge.Windows.Core;

namespace AirBridge.Windows.Services;

public sealed class FirewallStatus
{
    public FirewallStatus(bool transferRule, bool discoveryRule, string detail)
    {
        TransferRule = transferRule;
        DiscoveryRule = discoveryRule;
        Detail = detail;
    }

    public bool TransferRule { get; }
    public bool DiscoveryRule { get; }
    public bool IsReady => TransferRule && DiscoveryRule;
    public string Detail { get; }
}

/// <summary>
/// Applies the two inbound Windows Firewall rules AirBridge needs: one TCP rule
/// for the transfer listener, and one UDP rule so mDNS responses are not
/// silently dropped on private networks.
///
/// The rules are created by an elevated `netsh advfirewall` script. The app
/// itself never asks for elevation; it launches the script with a UAC prompt,
/// which is also what makes the change visible to the user.
/// </summary>
public sealed class FirewallService
{
    public const string TransferRuleName = "AirBridge File Transfer (TCP)";
    public const string DiscoveryRuleName = "AirBridge mDNS Discovery (UDP 5353)";

    private static readonly TimeSpan QueryTimeout = TimeSpan.FromSeconds(6);

    public string ScriptPath => Path.Combine(
        AppContext.BaseDirectory,
        "scripts",
        "Setup-Firewall.ps1");

    public string ReadmePath => Path.Combine(
        AppContext.BaseDirectory,
        "scripts",
        "README-Firewall.txt");

    /// <summary>Reads the current rule state without elevation.</summary>
    public async Task<FirewallStatus> GetStatusAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            var transfer = await RuleExistsAsync(TransferRuleName, cancellationToken).ConfigureAwait(false);
            var discovery = await RuleExistsAsync(DiscoveryRuleName, cancellationToken).ConfigureAwait(false);

            var detail = (transfer, discovery) switch
            {
                (true, true) => "Windows Firewall is open for AirBridge.",
                (false, false) => "No AirBridge firewall rules found.",
                (true, false) => "Inbound transfers are allowed; mDNS discovery is not.",
                (false, true) => "mDNS discovery is allowed; inbound transfers are not.",
            };

            return new FirewallStatus(transfer, discovery, detail);
        }
        catch (Exception error)
        {
            AppLog.Warn("Could not read the Windows Firewall state.", error);
            return new FirewallStatus(false, false, "Firewall state unknown.");
        }
    }

    /// <summary>
    /// Launches the setup script elevated. Returns null when the prompt was
    /// accepted, or a human-readable reason when it could not be launched.
    /// </summary>
    public string? LaunchSetup(int port)
    {
        var script = ScriptPath;
        if (!File.Exists(script))
        {
            return $"The firewall script was not found at {script}.";
        }

        try
        {
            var arguments =
                $"-NoProfile -ExecutionPolicy Bypass -File \"{script}\" -Port {port}";
            var start = new ProcessStartInfo("powershell.exe", arguments)
            {
                UseShellExecute = true,
                Verb = "runas",
                WindowStyle = ProcessWindowStyle.Normal,
            };

            Process.Start(start);
            return null;
        }
        catch (Exception error)
        {
            AppLog.Warn("The elevated firewall setup could not be launched.", error);
            return error is System.ComponentModel.Win32Exception win32 && win32.NativeErrorCode == 1223
                ? "The elevation prompt was cancelled."
                : Format.FriendlyException(error);
        }
    }

    private static async Task<bool> RuleExistsAsync(string ruleName, CancellationToken cancellationToken)
    {
        var start = new ProcessStartInfo("netsh.exe", $"advfirewall firewall show rule name=\"{ruleName}\"")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
            CreateNoWindow = true,
        };

        using var process = Process.Start(start);
        if (process is null)
        {
            return false;
        }

        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(QueryTimeout);

        try
        {
            await process.WaitForExitAsync(timeout.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException)
        {
            TryKill(process);
            return false;
        }

        // netsh exits non-zero when the named rule does not exist. Its output
        // is localized, so the exit code is the reliable signal.
        return process.ExitCode == 0;
    }

    private static void TryKill(Process process)
    {
        try
        {
            if (!process.HasExited)
            {
                process.Kill(entireProcessTree: true);
            }
        }
        catch
        {
            // Best effort.
        }
    }
}
