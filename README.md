# AirBridge

**Local-first, peer-to-peer file sharing for Android and Windows.**

AirBridge moves files directly between devices on the same network. There is no
cloud service, no account, and no upload: a Rust core handles discovery and
transfer, [UniFFI](https://mozilla.github.io/uniffi-rs/) exposes it to Kotlin and
C#, and each platform ships a native frontend — Jetpack Compose on Android and
WinUI 3 on Windows.

The repository contains the complete stack: the Rust engine, the UniFFI
boundary, hand-written coroutine/event adapters, and both desktop and mobile
applications.

> [!IMPORTANT]
> **Transfers are encrypted and authenticated.** Every connection runs the
> P-256 handshake exported by `libcore_ffi` before the file protocol starts;
> payload frames are protected with directional AES-256-GCM keys. Both apps show
> the real six-digit SAS from the handshake, and verified trust is stored against
> the peer's identity-key fingerprint. See
> [Security model and current limits](#security-model-and-current-limits).

## Contents

- [Architecture](#architecture)
- [Repository layout](#repository-layout)
- [Prerequisites](#prerequisites)
- [Quick start](#quick-start)
- [Build scripts](#build-scripts)
- [Using the bindings](#using-the-bindings)
- [How it works](#how-it-works)
- [Security model and current limits](#security-model-and-current-limits)
- [Troubleshooting](#troubleshooting)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [License](#license)

## Architecture

AirBridge is layered so that the protocol and cryptographic code stay
independent of any UI toolkit, executor, or operating system.

```text
┌──────────────────────────────────────────────────────────────────────┐
│                              Frontends                               │
│                                                                      │
│   Android                                 Windows                    │
│   Kotlin + Jetpack Compose                C# + WinUI 3               │
│   :app / :libcore-ffi                     AirBridge.Windows          │
└───────────────┬──────────────────────────────────┬───────────────────┘
                │ Kotlin Flow                      │ .NET events / Tasks
┌───────────────▼──────────────────────────────────▼───────────────────┐
│ bindings/          hand-written adapters + generated UniFFI glue     │
├──────────────────────────────────────────────────────────────────────┤
│ libcore_ffi        UniFFI 0.31 boundary, one shared Tokio runtime     │
├───────────────────┬───────────────────────┬──────────────────────────┤
│ libcore_crypto    │ libcore_discovery     │ libcore_transport        │
│ handshake, KDF,   │ BLE, mDNS/DNS-SD,     │ framing, 512 KiB chunks, │
│ AEAD frames, SAS  │ /24 subnet fallback   │ SHA-256, resume state    │
└───────────────────┴───────────────────────┴──────────────────────────┘
```

| Layer | Language / runtime | Responsibility |
| --- | --- | --- |
| `libcore_crypto` | Rust, no async runtime | UKEY2-derived handshake, persistent P-256 identities, HKDF-SHA512 key schedule, AES-256-GCM frames, Short Authentication String |
| `libcore_discovery` | Rust + Tokio | BLE beacons, mDNS/DNS-SD, concurrent subnet probe, tier coordination and peer table |
| `libcore_transport` | Rust + Tokio | Secure-channel handshake before `HELLO`, AEAD stream adapter, length-prefixed framing, chunk grid, per-chunk SHA-256, crash-safe resume, verified commit |
| `libcore_ffi` | Rust + UniFFI 0.31 | Pull-based discovery/transfer engines and event streams for Kotlin and C#; owns the single multi-threaded Tokio runtime |
| `bindings/kotlin` | Kotlin | Cold `Flow` adapters over the generated bindings |
| `bindings/csharp` | C# / .NET 8 | Event pumps and the `AirBridge.Core` wrapper assembly |
| `android/` | Kotlin, Compose, AGP 9 | Foreground service, BLE backend, MediaStore receive path, radar UI |
| `windows/` | C# / .NET 8, WinUI 3 | WinRT BLE backend, serial transfer queue, firewall setup, radar UI |

A typical transfer flows through the stack like this:

1. The platform BLE backend and the Rust mDNS/subnet tiers produce sightings.
2. `PeerDiscoveryEngine` deduplicates them into `PeerInfo` values keyed by a
   16-byte peer id and emits discovery events.
3. The app opens a `FileTransferSession` for the selected peer and port.
4. `libcore_transport` splits the file into checksummed chunks, resumes from the
   receiver's verified offset, and commits only after the whole-file digest
   matches.
5. Transfer and progress events are drained on the foreign side and rendered in
   the Compose or WinUI radar.

## Repository layout

```text
.
├── .gitattributes                 cross-platform line-ending rules
├── .gitignore                     build, IDE, OS and secret ignores
├── LICENSE                        Apache-2.0
├── Cargo.toml                     workspace manifest, pinned dependencies, lints
├── Cargo.lock                     committed for reproducible binaries
├── rust-toolchain.toml            stable toolchain + clippy + rustfmt
├── crates/
│   ├── libcore_crypto/            handshake, KDF, AEAD framing, SAS, tests
│   ├── libcore_discovery/         BLE, mDNS/DNS-SD, subnet scan, coordinator
│   ├── libcore_transport/         framing, chunk engine, resume, verification
│   └── libcore_ffi/               UniFFI surface + one Tokio runtime
├── bindings/
│   ├── kotlin/src/                Flow adapters over the generated bindings
│   └── csharp/                    .NET event pumps + AirBridge.Core.csproj
├── android/
│   ├── app/                       Compose application
│   ├── libcore-ffi/               AAR module, generated Kotlin, staged .so files
│   └── README.md                  Android build, permissions and design notes
├── windows/
│   ├── src/AirBridge.Windows/     unpackaged WinUI 3 application
│   ├── scripts/                   elevated firewall setup
│   └── README.md                  Windows build, BLE, firewall and design notes
├── scripts/                       cross-platform native/bindings build scripts
└── target/                        generated artifacts (ignored)
```

`target/bindings/kotlin` and `target/bindings/csharp` are produced by the build
scripts and are deliberately **not** committed. The same applies to the `.so`
files staged under `android/libcore-ffi/src/main/jniLibs/`.

## Prerequisites

| Tool | Version | Needed for |
| --- | --- | --- |
| Rust (rustup) | 1.88+ for the full workspace (the crypto core keeps a 1.74 MSRV) | All native builds |
| PowerShell | 7 (`pwsh`) recommended; Windows PowerShell 5.1 works | Build scripts on Windows |
| Visual Studio Build Tools | 2022, "Desktop development with C++" (MSVC linker) | Windows native build |
| .NET SDK | 8.0 | WinUI app and C# bindings |
| Windows | 10 version 1809 (build 17763) or newer, x64 | Windows app |
| JDK | 17 | Gradle / Android Gradle Plugin |
| Android SDK | API 36 (compileSdk 36.1) and build-tools 36.x | Android app |
| Android NDK | r28+ recommended (16 KB page compatibility) | Android native core |
| `cargo-ndk` | Latest (`cargo install cargo-ndk --locked`) | Android cross-compilation |
| Rust Android targets | `aarch64-linux-android`, `armv7-linux-androideabi`, `x86_64-linux-android`, `i686-linux-android` | Android native core |
| `uniffi-bindgen-cs` | `v0.11.1+v0.31.0` | C# bindings generation |

Notes:

- `rust-toolchain.toml` pins `stable` with `clippy` and `rustfmt`, so `rustup`
  installs the right components automatically.
- Windows App SDK 1.6 packages are restored through NuGet; the app is
  self-contained and needs no separate Windows App Runtime installer.
- Android builds use the checked-in Gradle 9.4.1 wrapper and AGP 9.2.1. The app
  targets API 36 and supports Android 8.0 (API 26) and newer.
- On Windows without the MSVC linker, the scripts accept a MinGW toolchain:
  `-Toolchain stable-x86_64-pc-windows-gnu`. See
  [Toolchain notes](#toolchain-notes-windows).

## Quick start

### 1. Build and test the Rust core

From the repository root:

```sh
cargo test
cargo clippy --all-targets
cargo fmt --all --check
```

The workspace is `unsafe`-free except for UniFFI's generated scaffolding in
`libcore_ffi`. Clippy and rustfmt are expected to be clean.

### 2. Windows (WinUI 3)

Build the native library, generate the C# bindings, and compile the wrapper
assembly:

```powershell
pwsh scripts/build-windows.ps1 -BuildWrapper
```

Then build the desktop app (the x64 platform is required because the native core
is x64):

```powershell
dotnet build windows/src/AirBridge.Windows/AirBridge.Windows.csproj `
  -c Release -p:Platform=x64
```

The output is a self-contained folder:

```text
windows/src/AirBridge.Windows/bin/x64/Release/
  net8.0-windows10.0.19041.0/win-x64/AirBridge.exe
```

Run `AirBridge.exe` from that folder. On first use, open the **Sharing** card and
set up the firewall rules, or run the elevated script directly from the
repository root:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\scripts\Setup-Firewall.ps1 -Port 42517
```

The script opens TCP 42517 (restricted to `AirBridge.exe` when it can locate it)
and UDP 5353 for mDNS, on **Private** and **Domain** networks only.

### 3. Android (Jetpack Compose)

Install the cross-compilation prerequisites once:

```powershell
cargo install cargo-ndk --locked
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android i686-linux-android
```

Cross-compile the native core, regenerate the Kotlin bindings, and stage the
`.so` files (skip the AAR for a normal app build):

```powershell
pwsh scripts/build-android.ps1 -NoAar
```

Build the debug APK:

```powershell
cd android
$env:JAVA_HOME = "$env:ProgramFiles\Android\Android Studio\jbr"
.\gradlew.bat :app:assembleDebug
adb install -r app\build\outputs\apk\debug\app-debug.apk
```

On macOS/Linux, the equivalent native step is:

```sh
./scripts/build-android.sh --no-aar
cd android
./gradlew :app:assembleDebug
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

The APK is written to `android/app/build/outputs/apk/debug/app-debug.apk`. If the
`.so` files are missing, Gradle still builds but the app shows an actionable
"native core could not be loaded" message at runtime instead of crashing. To
build a reusable AAR instead, run `scripts/build-android.ps1` (or
`scripts/build-android.sh`) without `-NoAar`/`--no-aar`.

### 4. First run

Put both devices on the same private network and keep the app foregrounded for
the first discovery pass. On Android, grant the Nearby devices/Bluetooth and
notification permissions. On Windows, accept the firewall prompt or run the
setup script above. Each device should appear on the other's radar within a few
seconds: mDNS is tried first, then the subnet fallback. The default transfer port
is `42517`.

### Build outputs

| Artifact | Path |
| --- | --- |
| Rust shared library (Windows) | `target/release/core_ffi.dll` |
| Generated C# bindings | `target/bindings/csharp/core_ffi.cs` |
| Generated Kotlin bindings | `target/bindings/kotlin/com/airbridge/core/core_ffi.kt` |
| Windows application | `windows/src/AirBridge.Windows/bin/x64/Release/net8.0-windows10.0.19041.0/win-x64/AirBridge.exe` |
| Android APK | `android/app/build/outputs/apk/debug/app-debug.apk` |
| Android AAR | `android/libcore-ffi/build/outputs/aar/libcore-ffi-release.aar` |

## Build scripts

All scripts operate from the repository root and expect the default in-repo
`target/` directory (`CARGO_TARGET_DIR` is not honoured).

| Script | What it does |
| --- | --- |
| `scripts/build-windows.ps1` | Host `core_ffi.dll` + C# bindings; `-BuildWrapper` also builds `AirBridge.Core` |
| `scripts/build-windows.sh` | The same on Linux/macOS (`.so`/`.dylib` host artifact) |
| `scripts/build-android.ps1` | Four-ABI `libcore_ffi.so` set + Kotlin bindings + optional AAR |
| `scripts/build-android.sh` | The same on Linux/macOS |

Useful flags:

```powershell
# Faster iteration: one ABI, debug profile, no AAR
pwsh scripts/build-android.ps1 -Abis arm64-v8a -Profile debug -NoAar

# Explicit NDK path
pwsh scripts/build-android.ps1 -NdkHome "C:\Android\Sdk\ndk\28.2.13676358"

# MinGW toolchain on Windows
pwsh scripts/build-windows.ps1 -Toolchain stable-x86_64-pc-windows-gnu -BuildWrapper
```

```sh
./scripts/build-android.sh --debug --abis arm64-v8a --no-aar
./scripts/build-windows.sh --build-wrapper
```

## Using the bindings

The FFI is pull-based: engines publish events and the foreign side requests them
one at a time. Hand-written adapters turn that into a Kotlin `Flow` or .NET
events.

### Kotlin

```kotlin
val engine = PeerDiscoveryEngine(DiscoveryConfig())
engine.start(bleBackend)                 // null is valid: the IP tiers carry on
engine.events().collect { event -> handle(event) }

val session = FileTransferSession.send("10.0.0.5:42517", path, SendOptions())
val report = session.run()               // progress arrives via session.events()
```

### C#

```csharp
var engine = new PeerDiscoveryEngine(new DiscoveryConfig());
var events = new PeerDiscoveryEvents(engine);
events.EventReceived += (_, e) => Console.WriteLine(e);
events.Start();
engine.Start(bleBackend: null);
```

`bindings/README.md` documents the adapter lifecycle and regeneration commands.
`crates/libcore_ffi/README.md` documents the memory model, threading contract,
cancellation semantics, and artifact verification.

## How it works

### Cryptographic core

`libcore_crypto` implements a UKEY2-derived handshake:

```text
M1  I -> R   ClientInit      version, random_c, suite, SHA-512 commitment to pk_c
M2  R -> I   ServerInit      version, random_s, suite, SHA-512 commitment to pk_s
M3  I -> R   ClientFinished  pk_c, identity_pk_c, signature_c
M4  R -> I   ServerFinished  pk_s, identity_pk_s, signature_s
```

The same four-message shape is used for transfers and for explicit pairing.
`pk_c`/`pk_s` are still checked against the commitments from M1/M2.
`signature_c`/`signature_s` cover the full transcript, the signer's ephemeral
public key, and the signer's identity key, so each side proves possession of
the long-lived identity bound to its fingerprint. The responder's transcript
already contains the initiator's key, so its signature commits to both.

Both parties then run P-256 ECDH and derive every session secret from the shared
secret and the full transcript `M1 | M2 | M3 | M4` with HKDF-SHA512:

- `AUTH_STRING`, an independent secret from which the Short Authentication
  String is derived;
- a key block yielding directional AES-256-GCM keys and nonce prefixes;
- a public `session_id` bound as associated data into every frame.

Frames are `version || direction || sequence || length || ciphertext || tag`,
with strictly increasing 64-bit sequence numbers that are authenticated as
associated data. A frame cannot be replayed, reflected, reordered, or moved
between sessions. Secret material is zeroised on drop and secret comparisons are
constant-time.

The SAS interface exposes 4-, 5-, or 6-digit codes. Two parties compare codes out
of band; because `AUTH_STRING` comes from a different HKDF extract than the frame
keys, displaying the SAS reveals nothing about the traffic keys. A
man-in-the-middle must keep both legs' SAS equal, which it cannot do except with
probability 10⁻ⁿ.

`libcore_crypto` is runtime-agnostic and `unsafe`-free. It is the part intended
to sit inside the trusted computing base, so it does not inherit an async
executor's threading model or cancellation semantics.

#### Threat model

Defended against:

- passive eavesdroppers (all traffic is AEAD-protected after the handshake);
- active man-in-the-middle attackers (detected by SAS comparison);
- message tampering and key substitution during the handshake (commitments and
  identity signatures);
- frame replay, reordering, reflection, and cross-session splicing.

Explicitly not defended against:

- **verified identity without the SAS**: the handshake proves possession of an
  identity key and transports its fingerprint, but a user must still compare
  the SAS (or an external credential) to bind that key to the person;
- denial of service: a peer can always refuse to complete a handshake;
- reordering networks: the replay policy is strict monotonic, not a sliding
  window. A transport that may deliver frames out of order must not use
  `FrameCipher` unmodified.

#### Divergence from Google's UKEY2

AirBridge speaks a UKEY2-*derived* protocol, not Google's wire format. The
differences are deliberate and documented in the crate-level rustdoc:

1. a **fourth message**, so that commitments are mutual — a server commitment is
   meaningless if the key is revealed in the same message;
2. **HKDF-SHA512**, as the specification's `P256_SHA512` suite requires (Google's
   C++ actually uses HKDF-SHA256, contradicting the suite it advertises);
3. a **13-byte salt**, as written in the specification (Google's C++ passes
   `sizeof(literal)`, silently including the trailing NUL);
4. a **canonical binary encoding** instead of protobuf, which has no canonical
   form and makes commitments over serialized bytes fragile.

Interoperating with Nearby Connections would require a compatibility layer; the
divergence points are isolated so that such work stays bounded.

### Peer discovery

`libcore_discovery` escalates through three tiers and feeds all of them into one
table keyed by a 16-byte `PeerId`:

1. **BLE** — advertising and scanning for service UUID `0xFE2C`. A beacon is only
   19 bytes and carries no IP address, so a BLE peer joins the table with an
   empty address list until another tier sees the same id.
2. **mDNS/DNS-SD** — the node advertises and browses `_airbridge._tcp.local.` on
   its own multicast socket. `hickory-proto` supplies the wire format; the crate
   is a responder as well as a browser.
3. **Subnet scan** — a concurrent HTTP GET of `/api/v1/register` against every
   host in the local `/24`, with a strict 1.5 s timeout. It runs only when
   configured as `Always` or as a `Fallback` after mDNS fails or stays silent past
   a grace period.

Platform BLE support is a seam (`BleBackend`), not a dependency: a binding plugs
in without touching the tiers, and `MockBleBackend` exercises the full lifecycle
on any host.

### File transport

`libcore_transport` moves one file over one reliable byte stream. Frames are a
4-byte big-endian length, a tag, and a body. Payloads are split into 512 KiB
chunks, each carrying its offset and a 32-byte SHA-256 digest. The receiver
persists its verified offset in a sidecar next to the part file, reports it in
the opening `ACK_OFFSET`, and the sender seeks directly to it. A bad chunk is
answered with a repair ack that rewinds to the last verified prefix, and the
whole-file digest is checked before the part file is renamed into place. The
sidecar and part file are untrusted hints: corruption can force a restart but
never a wrong result.

### FFI boundary

`libcore_ffi` exposes discovery and resumable transfer to Kotlin and C# through
UniFFI 0.31. The boundary is pull-based on purpose: the engines publish events,
and the foreign side drains them one at a time, so no Rust worker ever calls into
foreign code (except the platform BLE backend) and a slow consumer applies
backpressure instead of growing an unbounded queue.

- **One runtime, owned here.** A single two-worker Tokio runtime is created
  lazily for the process. Kotlin coroutines and .NET tasks only ever await a
  `JoinHandle`.
- **Dropping a future detaches.** Cancellation is explicit:
  `FileTransferSession.cancel()` and `PeerDiscoveryEngine.stop()`. Dropping a
  foreign future does not stop the Rust task.
- **One consumer per stream.** The Kotlin flows are cold; collect once and fan
  out in the app, or share the flow.
- **The BLE trait is the one Rust-to-foreign direction.** Implementations must be
  thread-safe and non-blocking. On Android, JNA attaches long-lived Tokio worker
  threads to the JVM on first callback.
- **Payload bytes never cross the boundary.** Files are read and moved on the
  Rust side; events carry metadata only.

## Security model and current limits

AirBridge is local-first by design and the transfer path is now encrypted and
authenticated end to end:

- **Authenticated handshake.** Every TCP session runs the P-256 handshake before
  `HELLO`. Each side signs the transcript with a persistent ECDSA P-256 identity
  key, and HKDF-SHA512 derives directional AES-256-GCM keys from the ephemeral
  shared secret and the full transcript.
- **SAS confirmation.** The six-digit code shown by both apps comes from the
  handshake, not from a local random generator. A man-in-the-middle cannot make
  both legs display the same code except with probability 10⁻⁶.
- **Fingerprint-based trust.** After approval, the peer's authenticated peer id
  is bound to the SHA-256 fingerprint of its identity key. Later transfers pin
  that key and auto-confirm only when the fingerprint is already trusted. Legacy
  peer-id-only trust entries are discarded.
- **Explicit pairing.** A pairing-only connection uses the same handshake and
  port. The receiver surfaces the SAS, reports a verified device, and re-arms
  without offering or saving a file.
- **Local unpairing.** **Forget this device** removes the peer-id-to-fingerprint
  binding and revokes the fingerprint on the current device. The identity key is
  kept, so the next connection requires a fresh SAS comparison. Trust is local,
  so the same action must be repeated on the other device for a full unpair.
- **Anonymous payload metadata.** The wire format carries no filename. Received
  files are staged in the app's private cache, shown for consent, and committed
  under a generated name with an extension guessed from content. The sender's
  authenticated peer id is available after the handshake.
- **Firewall scope.** The Windows rules are limited to Private and Domain
  profiles; public networks stay blocked.

Remaining limits: a peer that cannot complete the handshake is simply refused
(denial of service is always possible), and trust on first use still requires
the user to compare the SAS out of band. Replay protection is strict monotonic
per connection, which matches the ordered TCP stream.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Android: "native core could not be loaded" | Rebuild the `.so` for the device ABI (`adb shell getprop ro.product.cpu.abi`), reinstall the APK, and confirm the library is packaged. |
| Gradle cannot find the generated bindings | Run `pwsh scripts/build-android.ps1 -NoAar` (or `./scripts/build-android.sh --no-aar`) first. |
| No devices appear | Both devices on the same private LAN, BLE permissions granted, mDNS/subnet discovery enabled, and firewall rules present. Guest or AP-isolated Wi-Fi blocks mDNS and subnet probes. |
| Windows: `core_ffi.dll` not found | Run `pwsh scripts/build-windows.ps1` and rebuild the app so the DLL is copied beside `AirBridge.exe`. |
| Windows: app builds but will not start | Build with `-p:Platform=x64`; an AnyCPU build cannot load the x64 native core. |
| Windows: `Microsoft.Build.Packaging.Pri.Tasks.dll` error | Keep `EnableMsixTooling=true` in the project file; the app deliberately uses the Windows App SDK NuGet tasks. |
| Bluetooth tier is "Limited" | Bluetooth is off, the adapter is missing, permissions are denied, or the radio is reserved by another app. mDNS and subnet discovery continue. |
| Incoming transfers fail immediately | Check the receive port in Settings (default 42517), allow the app through the firewall, and make sure no other process owns the port. |
| Received files land in the source checkout | The default Windows receive folder is `%USERPROFILE%\Downloads\AirBridge`. If the repository is cloned to that exact path, Windows treats them as the same folder; change the download folder in Settings. |

Platform-specific details live in [`android/README.md`](android/README.md) and
[`windows/README.md`](windows/README.md).

## Roadmap

- Add `libcore_transfer`: multi-file manifests, queueing, and orchestration.
- Extend bindings to iOS/macOS, where the `staticlib`/`dylib` artifacts already
  build.

## Contributing

1. Fork the repository and create a focused branch.
2. Keep the Rust workspace warning-free:
   `cargo fmt --all --check`, `cargo clippy --all-targets`, `cargo test`.
3. Regenerate bindings with the build scripts; never edit files under
   `target/bindings/` by hand.
4. Keep `uniffi` and `uniffi-bindgen-cs` in lockstep. The current pair is UniFFI
   0.31 and `uniffi-bindgen-cs v0.11.1+v0.31.0`.
5. Describe user-visible changes in the pull request and update the relevant
   platform README when behaviour changes.

### Toolchain notes (Windows)

`rust-toolchain.toml` targets the platform-default target. On Windows that is
usually `x86_64-pc-windows-msvc`, which requires the Visual Studio C++ build
tools. If only a MinGW/TDM-GCC toolchain is available, build with the GNU host
target instead:

```powershell
cargo +stable-x86_64-pc-windows-gnu test
```

TDM-GCC distributions may ship without `libgcc_eh.a`. The unwinding symbols
(`_Unwind_*`) live in `libgcc.a`, so creating an empty archive where the linker
looks for it is sufficient. The build scripts accept
`-Toolchain stable-x86_64-pc-windows-gnu` for the same reason.

## License

Apache-2.0. See [`LICENSE`](LICENSE).
