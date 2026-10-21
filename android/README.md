# AirBridge for Android

The Android frontend for AirBridge: a Jetpack Compose app with a dark,
AirDrop-style radar, native peer discovery, and direct device-to-device file
transfers. It talks to the Rust engine through UniFFI and JNA, and keeps
discovery and transfers alive in a `connectedDevice` foreground service.

The UI and service are complete. One backend piece is still deliberately
stubbed because the current `libcore_ffi` surface does not expose it:

* the pairing sheet uses a **local preview SAS gateway** — it does not yet
  authenticate the remote device.

Inbound transfers are implemented: the service keeps a Rust receive session
bound to the transfer port and asks the user before copying a verified payload
into Downloads. See [Receiving files](#receiving-files).

## Modules

```text
android/
  settings.gradle.kts              includes :app and :libcore-ffi
  build.gradle.kts                 AGP 9.2.1, Compose compiler 2.2.10
  gradlew / gradlew.bat            Gradle 9.4.1 wrapper
  app/                             the Compose application
  libcore-ffi/                     Android library around the generated bindings
    src/main/jniLibs/<abi>/        libcore_ffi.so, staged by the build script
    src/main/AndroidManifest.xml   empty library manifest
  local.properties                 machine-specific SDK path (do not commit)
```

`:libcore-ffi` compiles Kotlin from two directories outside the module, so one
copy serves every consumer:

* `bindings/kotlin/src` — hand-written coroutine adapters;
* `target/bindings/kotlin` — generated UniFFI bindings (`com.airbridge.core`),
  produced by `uniffi-bindgen` and normalised for Kotlin 2.2+ by a Gradle task.

`:app` depends on the project module and also declares JNA `5.19.1@aar`
explicitly. Compose uses BOM `2026.02.01`; the app targets `compileSdk 36.1`,
`minSdk 26`, and `targetSdk 36`.

### Application layout

```text
app/src/main/java/com/airbridge/app/
  MainActivity.kt                 Compose entry point
  AirBridgeViewModel.kt           service binding, UI state, pickers, pairing
  core/
    AirBridgeService.kt           foreground service, notification, multicast lock
    AirBridgeController.kt        Rust discovery engine, BLE, transfer queue
    InboundReceiver.kt            bound receive sessions, consent, re-arming
    IncomingFileStore.kt          Downloads/MediaStore commit, sniffing, sidecar hints
    AndroidBleBackend.kt          advertising/scanning and sighting forwarder
    AppPreferences.kt             stable peer id, display name, trusted peers
    NativeCore.kt                 safe `libcore_ffi.so` availability check
    SasGateway.kt                 pairing seam + local preview implementation
  model/Models.kt                 UI models and formatting helpers
  ui/
    AirBridgeApp.kt               root composable, file/permission pickers
    IncomingSheet.kt              inbound consent card and circular progress ring
    RadarScreen.kt                header, status pills, file shelf, error banner
    RadarStage.kt                 radar canvas, peer bubbles, central node
    Sheets.kt                     peer, pairing, profile, permission sheets
    Components.kt                 glass surfaces, buttons, shared drawing helpers
    theme/Theme.kt                colour palette and typography
```

## Building

### 1. Native core and Kotlin bindings

The APK does not ship a prebuilt shared library. Build it first from the
repository root.

Prerequisites:

* Rust targets for the ABIs you need (`aarch64-linux-android`,
  `armv7-linux-androideabi`, `x86_64-linux-android`, `i686-linux-android`);
* `cargo-ndk` (`cargo install cargo-ndk --locked`);
* an Android SDK with an NDK; set `ANDROID_NDK_HOME` / `ANDROID_NDK_ROOT`, or
  pass `-NdkHome` on Windows.

```powershell
# Cross-compile, stage libcore_ffi.so, regenerate Kotlin bindings
pwsh scripts/build-android.ps1 -NoAar

# Or also assemble the reusable AAR
pwsh scripts/build-android.ps1
```

```sh
./scripts/build-android.sh          # see the script for flags
```

To build one ABI while iterating:

```powershell
pwsh scripts/build-android.ps1 -Abis arm64-v8a -Profile debug -NoAar
```

The script stages libraries at
`android/libcore-ffi/src/main/jniLibs/<abi>/libcore_ffi.so`, regenerates
`target/bindings/kotlin/com/airbridge/core/core_ffi.kt`, and applies the same
Kotlin 2.2 compatibility patch as the Gradle task. The patch is idempotent and
only affects the generated error classes.

> Apps that target Android 15+ should build the native library with a
> 16 KB-page-compatible NDK/toolchain (NDK r28 or newer, or an explicit
> `-Wl,-z,max-page-size=16384`). This is a native build concern; the Gradle
> configuration does not add linker flags for Rust.

### 2. App

```powershell
cd android
$env:JAVA_HOME = "$env:ProgramFiles\Android\Android Studio\jbr"
.\gradlew.bat :app:assembleDebug
```

```sh
cd android
./gradlew :app:assembleDebug
```

The debug APK is written to
`android/app/build/outputs/apk/debug/app-debug.apk`.

Release builds enable R8 and resource shrinking:

```powershell
.\gradlew.bat :app:assembleRelease
```

Add a signing configuration in `app/build.gradle.kts` (or use Android Studio's
generated one) before distributing the release APK.

If the `.so` files are missing, the Gradle build prints a warning but still
succeeds. At runtime `NativeCore.status()` catches the
`UnsatisfiedLinkError` and the radar shows an actionable setup message instead
of crashing.

## How it works

### Discovery

`AirBridgeController` creates a `PeerDiscoveryEngine` with:

| Setting | Value |
| --- | --- |
| peer id | stable 16-byte id persisted in shared preferences |
| display name | editable in the profile sheet, default is the device model |
| listen port | `42517` |
| BLE | enabled |
| mDNS | enabled |
| subnet policy | `FALLBACK` |

Android BLE is implemented in `AndroidBleBackend`. It advertises and scans for
service UUID `0xFE2C`; the service data is the 16-byte peer id followed by the
2-byte big-endian transfer port. Observations are forwarded to the Rust
coordinator through `BleObservationSink`. The Rust mDNS and subnet tiers run
unchanged inside the native core.

Discovery events update a `StateFlow` of `NearbyPeer` values. A periodic snapshot
refresh backs up the event stream, and peers are deduplicated by id. Tier state
and tier errors feed the status pills and the error banner.

### Foreground service

`AirBridgeService`:

* runs in the foreground with type `connectedDevice`;
* holds a `WifiManager.MulticastLock` for mDNS;
* mirrors discovery and transfer state into an ongoing notification with a
  **Stop** action;
* owns `AirBridgeController`, which keeps discovery, the inbound receive
  listener, and transfers alive across activity recreation and backgrounding;
* releases the multicast lock and shuts the controller down in `onDestroy`.

The activity starts and binds to the service. The ViewModel observes controller
flows, reconnects with bounded retries if the binding dies, and never creates a
second discovery engine.

Notification permission is requested on Android 13+. If it is denied the
service still runs; Android simply hides the notification from the drawer.

### Sending files

1. Pick files with the system document picker, or drag document URIs onto the
   radar. Read access is persisted with `takePersistableUriPermission`.
2. Tap a peer. With files selected, the radar sends immediately to a trusted
   peer, or opens the PIN sheet for a new peer. Without files it opens the peer
   sheet.
3. Files picked with the system picker keep read access through
   `takePersistableUriPermission` when the provider grants it. Each file is
   copied from its content URI into the app cache before the Rust
   `FileTransferSession` is opened. Multiple files are sent sequentially and the
   cache copy is deleted afterwards.
4. The central avatar draws a circular progress ring; the notification mirrors
   the same state. A partial wake lock is held for the duration of a transfer.
5. Cancellation calls `FileTransferSession.cancel()` and moves the UI to
   `CANCELLING` until the session reports its terminal state.

The Rust `Progress` event counts framing bytes as well as file bytes, so the UI
clamps the reported ratio to `0..1`.

### Receiving files

`AirBridgeController` owns an `InboundReceiver` for the whole service lifetime.
It keeps a `FileTransferSession.receive("0.0.0.0:42517", …)` bound while the
service runs, accepts one peer at a time, and re-arms after each session. The
transport exposes no pre-accept metadata callback, so the receiver:

1. stages the payload in the private cache (`cacheDir/incoming`);
2. surfaces an `IncomingTransferUi` as soon as Rust reports `Started`;
3. shows `IncomingTransferSheet` with Accept/Decline in Compose;
4. only after consent and end-to-end verification copies the file into public
   `Downloads/AirBridge` — through `MediaStore.Downloads` on API 29+, and the
   legacy public directory (with `WRITE_EXTERNAL_STORAGE`) on API 26–28.

Declining, or letting the 120-second consent window expire, cancels the Rust
session and deletes the `.part`/`.part.state` files. A partial wake lock is held
only while frames are actually arriving. The sheet's ring follows
`TransferEvent.Progress`; because receive events do not carry the `HELLO` size,
the adapter reads `total_len` from the transport's `ABRS` resume sidecar and
stays indeterminate until it is available.

### Identity and trust

`AppPreferences` stores a random 32-hex-character peer id (generated with
`SecureRandom` if the native engine does not provide one), the display name, and
the set of locally trusted peer ids. Trusted peers get a mint ring and a check
badge in the radar and in the peer sheet.

## Permissions

Declared in `app/src/main/AndroidManifest.xml`:

| Permission | Why |
| --- | --- |
| `INTERNET`, `ACCESS_NETWORK_STATE` | local sockets, subnet discovery |
| `ACCESS_WIFI_STATE`, `CHANGE_WIFI_STATE`, `CHANGE_NETWORK_STATE` | Wi-Fi state and foreground-service prerequisites |
| `CHANGE_WIFI_MULTICAST_STATE` | the multicast lock needed by mDNS |
| `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_CONNECTED_DEVICE` | background discovery/transfers |
| `WAKE_LOCK` | transfer wake lock |
| `POST_NOTIFICATIONS` | ongoing status notification (Android 13+) |
| `WRITE_EXTERNAL_STORAGE` (max SDK 28) | legacy `Downloads/AirBridge` writes on Android 8–9 |
| `BLUETOOTH_SCAN`, `BLUETOOTH_ADVERTISE`, `BLUETOOTH_CONNECT` | BLE discovery (Android 12+) |
| `ACCESS_FINE_LOCATION` (max SDK 30) | legacy BLE scanning |
| `BLUETOOTH`, `BLUETOOTH_ADMIN` (max SDK 30) | legacy Bluetooth |
| `NEARBY_WIFI_DEVICES` | declared for nearby Wi-Fi access |

At runtime the app requests the BLE permissions (or fine location before
Android 12) and notification permission. On Android 8–9, saving a received file
also asks for storage access; API 29+ uses MediaStore and needs no grant. If
nearby access is denied, discovery still tries the IP tiers where possible and
the profile sheet links to system settings.

The foreground service is started only after the activity is visible, which
avoids Android 12+ background-start restrictions. If the platform refuses to
promote it to the foreground, the service logs the failure and continues while
bound.

## Known limitations

### Pairing is a UI preview

The current FFI exposes discovery and transfer sessions only; the Rust SAS
handshake is not exported. `SasGateway` is the seam for the real handshake, and
`LocalPreviewSasGateway` supplies a six-digit code locally so the sheet and its
state machine can be exercised.

The preview:

* generates a random code on this device and approves it after a short delay;
* does not exchange a commitment or nonce with the peer;
* does **not** authenticate anyone, and deliberately does **not** add the peer
  to the locally trusted set.

The pairing sheet displays a warning explaining this. To make verification
real, implement `SasGateway` on top of the Rust handshake and inject it into
`AirBridgeController`; only return `true` from `approve` once both devices have
derived the same code. A real implementation should leave `isPreview` at its
default `false` so a successful check can persist trust.

### Inbound payload metadata

The AirBridge wire protocol carries no filename or peer identity, so the
consent card says "Incoming file" and a timestamped name is generated after the
payload is verified. Only one inbound offer is handled at a time; a verified
file waiting for Save/Discard keeps the listener from re-arming. The sender is
not asked before bytes start arriving — they are confined to the app cache until
the user accepts.

### Other notes

* The subnet scan probes the local `/24` and can take a few seconds on networks
  that block discovery traffic.
* BLE sightings carry no IP address. A peer becomes sendable once mDNS or the
  subnet tier supplies an address.
* `local.properties` contains a machine-specific SDK path and should not be
  committed.

## Troubleshooting

**"The native core could not be loaded"** — no `libcore_ffi.so` was packaged
for the device ABI, or JNA could not extract `libjnidispatch`. Run the native
build with the ABI shown by `adb shell getprop ro.product.cpu.abi` and reinstall
the app. Verify the APK with:

```powershell
unzip -l app/build/outputs/apk/debug/app-debug.apk | Select-String libcore_ffi
```

**Bluetooth tier reports a permission error** — grant Nearby devices /
Bluetooth permissions, and make sure Bluetooth is switched on.

**mDNS finds nothing but subnet discovery eventually does** — some access points
filter multicast. Keeping the app foregrounded gives discovery more time, but
the service is designed to keep the multicast lock while it runs.

**Gradle cannot find the generated bindings** — run
`pwsh scripts/build-android.ps1 -NoAar` (or the shell equivalent) first. The
`:libcore-ffi:checkGeneratedBindings` task fails with the expected path when
they are missing.

## License

Apache-2.0.
