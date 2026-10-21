# AirBridge for Windows

The Windows desktop frontend for AirBridge: an unpackaged WinUI 3 app with a
dark, AirDrop-style radar, native Bluetooth LE discovery, and direct
device-to-device file transfers. It talks to the Rust engine through the
generated UniFFI C# bindings and `core_ffi.dll`.

```text
windows/
  src/AirBridge.Windows/
    App.xaml / App.xaml.cs              application entry point
    MainWindow.xaml / .xaml.cs          shell, custom title bar, dialogs, pickers
    Themes/AirBridgeTheme.xaml          palette, cards, buttons, converters
    Controls/PeerRadarView.xaml         interactive radar and drop target
    Controls/RadarPanel.cs              orbit layout for peer bubbles
    Dialogs/SasVerificationDialog.xaml  SAS and key-fingerprint confirmation
    Dialogs/SettingsDialog.xaml         device, port, folder, discovery settings
    Models/                             peer, staged-file and transfer view models
    Services/
      NativeCore.cs                     safe native-library discovery and loading
      DiscoveryService.cs               PeerDiscoveryEngine lifecycle and events
      WindowsBleBackend.cs              WinRT BLE advertiser/scanner
      TransferCoordinator.cs            serial send queue and secure receive loop
      SasService.cs                     compare-the-code prompt model
      FirewallService.cs                firewall rule status and elevated setup
      SettingsStore.cs                  settings and identity in %LOCALAPPDATA%
    ViewModels/MainViewModel.cs         UI state and orchestration
  scripts/Setup-Firewall.ps1            elevated netsh advfirewall setup
  scripts/README-Firewall.txt           firewall usage notes
```

## Requirements

* Windows 10 version 1809 (build 17763) or newer, x64.
* [.NET 8 SDK](https://dotnet.microsoft.com/download/dotnet/8.0) to build, and
  the .NET 8 Desktop Runtime to run.
* A Rust toolchain with the MSVC target, plus the generated UniFFI C# bindings.
  See the repository's `scripts/build-windows.ps1`.
* The Windows App SDK 1.6 NuGet packages. They are restored automatically; the
  app is **self-contained** for the Windows App SDK, so no separate Windows App
  Runtime installer is needed.
* Bluetooth LE is optional. Discovery falls back to mDNS and the subnet sweep
  when the radio is missing or turned off.

## Build

### 1. Native core and generated bindings

From the repository root:

```powershell
pwsh scripts/build-windows.ps1 -BuildWrapper
```

This produces:

* `target/release/core_ffi.dll` (x64);
* `target/bindings/csharp/core_ffi.cs`;
* the `AirBridge.Core` wrapper assembly.

The app project copies `core_ffi.dll` next to `AirBridge.exe` at build time.

### 2. Desktop app

```powershell
dotnet build windows/src/AirBridge.Windows/AirBridge.Windows.csproj `
  -c Release -p:Platform=x64
```

`-p:Platform=x64` is required: the native library is x64 and an unpackaged
WinUI app needs a concrete runtime identifier. Building from Visual Studio?
Open the project and select **x64** in the platform dropdown.

The output is:

```text
windows/src/AirBridge.Windows/bin/x64/Release/
  net8.0-windows10.0.19041.0/win-x64/AirBridge.exe
```

Run `AirBridge.exe` from that folder. The whole folder is the application; no
installer or MSIX registration is needed.

> `EnableMsixTooling=true` in the project file is intentional. It pulls the
> Windows App SDK's own MSIX/PRI MSBuild tasks from NuGet, which keeps
> `dotnet build` working without the Visual Studio UWP workload.

## Using the app

* **Nearby** shows discovered devices on the radar and in the grid below it.
  Click a device (or its bubble) to select it as the send target.
* **Send** accepts files dropped anywhere on the radar, dropped on the window,
  or chosen with the file picker. Files are staged locally; **Clear** removes
  them.
* **Verify pairing** opens the security-code dialog. Both devices show the
  same six-digit code from the P-256 handshake; approving it binds the other
  device's key fingerprint to its peer id. An already trusted device also has
  **Verify device again**, which re-runs the handshake and replaces the pinned
  fingerprint if the other side legitimately rotated its key, and **Forget this
  device**, which removes the local binding. See
  [Secure pairing](#secure-pairing) below.
* **Transfers** shows one circular progress ring per file. Pause, resume,
  cancel, dismiss, and *Show in folder* are available as the transfer allows.
* **Sharing** turns the inbound listener on or off and shows the transfer port
  and firewall state. **Set up** launches the elevated firewall script.
* **Settings** changes the display name, transfer port, download folder,
  launch behaviour, and discovery tiers. The peer ID is assigned on the first
  scan and persisted.

### Where data lives

| Item | Path |
| --- | --- |
| Settings | `%LOCALAPPDATA%\AirBridge\settings.json` |
| Device identity | `%LOCALAPPDATA%\AirBridge\identity.pkcs8` |
| Logs | `%LOCALAPPDATA%\AirBridge\logs\airbridge-YYYYMMDD.log` |
| Received files | `%USERPROFILE%\Downloads\AirBridge` by default |

Change the download folder in **Settings**. The **Downloads** and **Logs**
buttons on the Sharing card open both folders in File Explorer.

## How it works

### Discovery

`DiscoveryService` creates a Rust `PeerDiscoveryEngine` with:

| Setting | Default |
| --- | --- |
| peer id | stable 16-byte id generated when settings are first created |
| display name | `<user>'s PC`, editable |
| listen port | `42517` |
| BLE | enabled |
| mDNS | enabled |
| subnet policy | `Fallback` |

mDNS and the subnet probe run inside the Rust core. The Windows layer only
implements the BLE seam.

`WindowsBleBackend` advertises and scans for service UUID `0xFE2C`. The
service data is the 16-byte peer id followed by the 2-byte big-endian transfer
port. Windows repeats the 16-bit UUID inside the service-data section. The
backend also accepts the Rust coordinator's versioned 19-byte manufacturer-data
beacon (`01 | id[16] | port[2]`) so both encodings interoperate.

Scanning starts automatically on launch unless **Scan for devices on launch**
is off; **Rescan** starts it on demand. Incoming transfers are independent of
discovery and keep listening as long as **Accept incoming files** is on.

### Transfers

`TransferCoordinator` owns every `FileTransferSession`:

* outgoing files are sent **one at a time** in a serial queue;
* the receiver runs a long-lived accept loop on the configured port;
* progress is marshalled to the UI thread and shown in the ring;
* pause cancels the running session, keeps the receiver's part file, and
  resumes with the *same payload id* so the sender seeks to the receiver's
  verified prefix instead of starting over;
* incoming connections that do not contain a valid AirBridge frame (port
  scanners, HTTP probes) are ignored instead of being shown as failed
  transfers.

### Secure pairing

Every transfer starts with a P-256 key-exchange handshake, before the file
protocol speaks. Each device signs the transcript with a long-lived P-256
identity key; HKDF then derives independent AES-256-GCM keys for each
direction. The six-digit code on both screens is the SAS derived from that
transcript, so a man-in-the-middle cannot make the two codes match.

Trust is keyed by the SHA-256 fingerprint of the peer's identity key, not by
the advertised peer id alone. Approving the code stores that fingerprint and
binds the authenticated peer id to it; later transfers pin the key and
auto-confirm only when the fingerprint is already trusted. Legacy
`trustedPeers` entries from older builds are ignored because an unauthenticated
id proves nothing.

**Forget this device** removes the selected device's peer-id-to-fingerprint
binding and revokes the fingerprint when no other peer uses it. The identity key
is kept, so a later connection shows the SAS again. Trust is stored locally on
each side; repeat the action on the other device for a full unpair.

Explicit pairing uses the same handshake over the transfer port, with a
pairing-only preamble. The receiver reports a verified device without
offering or saving an empty file, then re-arms its listener. Incoming
connections that cannot complete the handshake or present an unusable frame
(port scanners, HTTP probes) are ignored instead of being shown as failed
transfers.

### Firewall

Windows Firewall drops unsolicited inbound traffic by default. The app checks
for two `netsh advfirewall` rules on the Sharing card:

* **AirBridge File Transfer (TCP)** for the receive port, restricted to
  `AirBridge.exe` when the script can locate it;
* **AirBridge mDNS Discovery (UDP 5353)** so multicast replies reach the app.

Both rules are scoped to the **Private** and **Domain** profiles. Public
networks are left blocked. The elevated script is copied beside the app as
`scripts/Setup-Firewall.ps1`; it can also be run by hand:

```powershell
powershell -ExecutionPolicy Bypass -File windows/scripts/Setup-Firewall.ps1 -Port 42517
powershell -ExecutionPolicy Bypass -File windows/scripts/Setup-Firewall.ps1 -Remove
```

## Known limitations

### Incoming file names

The transport moves an anonymous byte stream and does not carry a filename.
Received files are committed under a generated name (timestamp plus payload
id), then renamed after verification with an extension guessed from the first
bytes (PNG, JPEG, PDF, ZIP, MP4, plain text, and so on). Unknown content keeps
a `.bin` extension. The friendlier name is shown in the transfer list and in
the Downloads folder.

### Pause is resume-by-reconnect

The FFI has `Cancel()` but no freeze primitive. Pausing ends the current TCP
session; resuming creates a new `FileTransferSession` with the original payload
id. This is what makes pause genuinely resumable, but the peer must still be
reachable when **Resume** is pressed.

### Single inbound transfer

The receiver accepts one connection per session and then rebinds. Two senders
targeting the same device are serialized naturally; a second connection may be
refused while the first is active. Outbound files are queued by this app.

### Windows BLE

The WinRT publisher is asynchronous: `Start()` can succeed and the radio can
then abort with `RadioNotAvailable`. The app retries with backoff and keeps
mDNS/subnet discovery running. If the machine has no Bluetooth adapter, the
Bluetooth tier stays **Limited** and the other tiers continue.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `core_ffi.dll` not found at startup | Run `pwsh scripts/build-windows.ps1`; rebuild the app so the DLL is copied beside `AirBridge.exe`. |
| Build error about `Microsoft.Build.Packaging.Pri.Tasks.dll` | Keep `EnableMsixTooling=true`; the project deliberately uses the Windows App SDK NuGet tasks. |
| Build copies the exe but the app will not start | Build with `-p:Platform=x64`; an AnyCPU build cannot load the x64 native core. |
| No devices appear | Confirm both apps are on the same private network, mDNS/subnet discovery is enabled, and the firewall rules exist. A guest/AP-isolated Wi-Fi network blocks both mDNS and subnet probes. |
| Bluetooth tier says Limited | Bluetooth is off, the adapter is missing, or the radio is reserved by another app. mDNS and the subnet sweep still work. |
| Incoming transfers fail immediately | Check the port in Settings, allow the app through Windows Firewall, and make sure no other process is bound to the port. |
| Firewall status does not change after Set up | Accept the UAC prompt, wait for the script to finish, then press the refresh button next to **Windows Firewall**. |
| Received files appear inside the source checkout | The default receive folder is `%USERPROFILE%\Downloads\AirBridge`. If the repository is cloned to that exact path, Windows treats it as the same folder (paths are case-insensitive). Change **Download folder** in Settings or move the checkout. |

For diagnostics, open **Logs** in the Sharing card. Logs are also written under
`%LOCALAPPDATA%\AirBridge\logs`.

## Security notes

AirBridge is local-first: no cloud service, no account, and no uploads. Every
transfer uses an authenticated P-256 handshake followed by directional
AES-256-GCM frames, and trust is bound to the peer's identity-key fingerprint
after the user compares the SAS. Firewall rules are limited to private and
domain networks, and the transfer rule is bound to the app executable when
possible. The identity private key stays in
`%LOCALAPPDATA%\AirBridge\identity.pkcs8` and is never sent to another device.
