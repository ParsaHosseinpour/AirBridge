# libcore_ffi

The AirBridge FFI boundary. It exposes peer discovery and resumable file
transfer to Kotlin (coroutine flows) and C# (tasks and events) through
[UniFFI](https://mozilla.github.io/uniffi-rs/) 0.31.

The library name is `core_ffi`, so the artifacts are:

| platform | artifact                       |
|----------|--------------------------------|
| Windows  | `core_ffi.dll`                 |
| Android  | `libcore_ffi.so` (`jniLibs/`)  |
| iOS/macOS| `libcore_ffi.a`/`libcore_ffi.dylib` (for a future binding) |

This crate is deliberately thin. It contains no protocol logic: it wraps
`libcore_discovery` and `libcore_transport`, adapts their types to the FFI,
and owns the one Tokio runtime the async exports need. Every hand-written
line in `src/` is safe Rust; the only `unsafe` in the crate is what UniFFI's
generated scaffolding requires (the workspace-level `unsafe_code = "forbid"`
is overridden locally for exactly that reason).

## Surface

| exported item                  | kind              | foreign view                        |
|--------------------------------|-------------------|-------------------------------------|
| `PeerDiscoveryEngine`          | object            | `PeerDiscoveryEngine`                |
| `FileTransferSession`          | object            | `FileTransferSession`                |
| `BleBackend`                   | foreign trait     | interface to implement on the platform |
| `BleObservationSink`           | object (Rust)     | handed to the `BleBackend` implementation |
| `BleRequest`                   | record            | value                                |
| `DiscoveryConfig`, `PeerInfo`  | records           | value                                |
| `SendOptions`, `ReceiveOptions`, `TransferReport` | records | value                       |
| `DiscoveryEvent`, `TransferEvent` | enums          | sealed classes / records             |
| `TierState`, `DiscoveryTier`, `SubnetPolicy`, `TransferDirection`, `TransferStatus`, `AbortReason` | enums | value enums |
| `FfiError`                     | error enum        | exception hierarchy                  |

Both engines are pull-based: `nextEvent()` resolves to the next event, or
`None` once the underlying stream is closed. The hand-written adapters in
`bindings/` turn that into what each platform expects:

| platform | adapter                          | result                          |
|----------|----------------------------------|---------------------------------|
| Kotlin   | `bindings/kotlin/src/com/airbridge/core/Events.kt` | cold `Flow` per stream |
| C#       | `bindings/csharp/*Events.cs`     | .NET events raised by a pump task |

Pull instead of push is deliberate. A push-style callback interface would
let the Rust side call arbitrary foreign code from a Tokio worker at any
time, which forces every listener to be thread-safe and, on Android, forces
JNA to attach the worker thread to the JVM on its first callback. Polling
keeps all foreign execution on the foreign executor and gives the consumer
natural backpressure: the next event is requested only after the previous
one was handled.

## Concurrency model

* **One runtime, owned here.** `src/runtime.rs` lazily creates a single
  two-worker multi-threaded Tokio runtime for the process. Kotlin coroutines
  and .NET tasks never need to provide a runtime.
* **Thin async shells.** Every exported `async fn` clones its state, spawns
  the real work onto that runtime, and awaits the `JoinHandle`. The future
  the foreign executor polls therefore only ever awaits a handle; it never
  performs I/O on the foreign scheduler.
* **Dropping a foreign future detaches, it does not stop.** A cancelled
  Kotlin `await` or a disposed .NET task leaves the spawned Rust task
  running. Stopping work is explicit: `FileTransferSession.cancel()` or
  `PeerDiscoveryEngine.stop()`.
* **The BLE trait is the one Rust-to-foreign direction.** `BleBackend.start`,
  `stop` and `isRunning` are invoked from Tokio workers (and `observe` from
  whatever thread the platform radio uses). Implementations must be
  thread-safe, and `start`/`stop` must not block for long: the coordinator
  supervises the tier on its worker.
* **One consumer per stream.** `nextEvent()` serializes concurrent calls
  with an async mutex, but the contract is one drain loop per session. The
  Kotlin flows are cold: collecting the same engine's flow twice splits the
  events between the collectors. Collect once and fan out in the app, or use
  `shareIn`.

## Cancellation

UniFFI has no built-in cancellation, so it is explicit:

* `FileTransferSession.cancel()` flips a flag and wakes the poll currently
  suspended on the socket (`src/cancel.rs`). The wrapped stream then fails
  its next poll, the transport unwinds through its normal error path — its
  ack pump is aborted, the receiver's part file and sidecar survive — and
  `run()` resolves to `FfiError::Cancelled`.
* `PeerDiscoveryEngine.stop()` asks the coordinator to shut all tiers down.
  It is non-blocking and idempotent; the coordinator publishes
  `DiscoveryEvent::Shutdown` and only then closes the stream, so a UI that
  is draining `nextEvent` still observes the terminal event.

Cancel is prompt while connecting, accepting, reading and writing. The final
`COMPLETE` handshake of a transfer is bounded by the configured response
timeout, not by cancellation.

## Memory model

Only owned, copyable values cross the boundary:

* **Strings and byte arrays are copied.** UniFFI lowers an owned Rust
  `String` or `Vec<u8>` into a `RustBuffer` — a Rust-allocated block — and
  copies it into a Kotlin `String`/`ByteArray` or a C# `string`/`byte[]`.
  The buffer is freed by the Rust allocator through the generated cleaner,
  never by the foreign garbage collector. There is no shared slice whose
  lifetime anyone has to reason about.
* **Payload bytes never cross.** A transfer reads the file on the Rust side
  and moves it between sockets; progress is a pair of integers, and events
  carry metadata only.
* **Records are copies, objects are shared handles.** A `PeerInfo` or a
  `TransferReport` is a value: mutating your copy cannot affect the engine.
  An object (`PeerDiscoveryEngine`, `FileTransferSession`,
  `BleObservationSink`) is a reference-counted handle; the Kotlin cleaner or
  the C# finalizer releases one Rust reference.
* **Peer ids and paths are strings.** A peer id crosses as 32 lowercase hex
  digits, not as bytes: Kotlin `ByteArray` breaks `data class` equality and
  C# `byte[]` compares by reference, so an id kept as a byte array would
  silently fail lookups. Decode with `decodeHex`/`Convert.FromHexString`
  when canonical bytes are genuinely needed.
* **Immutability on the foreign side.** `uniffi.toml` generates immutable
  Kotlin records; a snapshot that was emitted as an event is never the
  engine's own state.
* **UTF-8 at the boundary.** Rust `String`s are always valid UTF-8. Kotlin
  decoding replaces malformed sequences; C# marshals UTF-8 and produces
  `null` only for `Option<String>`.

## Threading and JNA (Android)

The Kotlin bindings load the library with JNA. Two consequences:

1. Calls from Kotlin into Rust are ordinary JNI calls; any thread (including
   a `Dispatchers.IO` worker) may make them.
2. Calls from Rust into Kotlin — `BleBackend.start`/`stop`/`isRunning` —
   arrive on Tokio workers. The first callback on each such thread makes JNA
   attach the thread to the JVM (a one-time cost per thread; the threads are
   long-lived, so it is paid at most once per worker). If a callback must
   run on a specific thread (to touch Android UI, or to register a
   `BluetoothLeAdvertiser` whose callbacks need a `Looper`), marshal inside
   the Kotlin implementation before returning.

Keep `BleObservationSink.observe` cheap and non-blocking: the coordinator
forwards observations into a channel, so a slow sink stalls the radio
callback thread, not a runtime worker.

## Building

Requirements common to all targets: a Rust toolchain (the crate needs 1.88+
for UniFFI 0.31) and, for bindings, the generator for your language.

### Bindings

Kotlin bindings come from the in-tree generator:

```text
cargo run -p libcore_ffi --features cli --bin uniffi-bindgen -- \
    generate --library target/release/core_ffi.dll \
    --language kotlin --out-dir target/bindings/kotlin \
    --config crates/libcore_ffi/uniffi.toml --no-format
```

C# bindings come from `uniffi-bindgen-cs`, pinned to the tag that targets
UniFFI 0.31:

```text
cargo install uniffi-bindgen-cs --git https://github.com/NordSecurity/uniffi-bindgen-cs --tag v0.11.1+v0.31.0
uniffi-bindgen-cs --library target/release/core_ffi.dll \
    --out-dir target/bindings/csharp --config crates/libcore_ffi/uniffi.toml --no-format
```

The generator reads the API metadata out of the compiled library, so the
library must be built first. Use the host artifact for this step even when
you are cross-compiling the shipping artifacts.

Both commands are wrapped by `scripts/build-windows.ps1` / `.sh` (host
library + C# bindings) and `scripts/build-android.ps1` / `.sh` (NDK build +
Kotlin bindings + AAR). See `scripts/README.md`.

### Windows DLL

```powershell
pwsh scripts/build-windows.ps1          # target/release/core_ffi.dll + C# bindings
pwsh scripts/build-windows.ps1 -BuildWrapper   # also `dotnet build` bindings/csharp/AirBridge.Core.csproj
```

The MSVC linker is not always installed; on machines with only a MinGW/TDM
GCC toolchain, pass `-Toolchain stable-x86_64-pc-windows-gnu` (the same
caveat the workspace README documents).

### Android AAR

```sh
scripts/build-android.sh                # four ABIs, Kotlin bindings, gradle :libcore-ffi:assembleRelease
```

Requires `cargo-ndk`, the four Android Rust targets, an Android SDK and NDK
(`ANDROID_NDK_HOME` or `ANDROID_HOME`), and either an `android/gradlew`
wrapper or `gradle` on `PATH`. The script copies the generated `.so` files
into `android/libcore-ffi/src/main/jniLibs/<abi>/` and the generated Kotlin
into `target/bindings/kotlin/com/airbridge/core/`, both of which the Gradle
module uses as source directories, then assembles
`android/libcore-ffi/build/outputs/aar/libcore-ffi-release.aar`.

The AAR depends on `net.java.dev.jna:jna:5.14.0@aar` and exposes
`kotlinx-coroutines-core` (the generated suspend functions need it on the
consumer's classpath).

## Verifying an artifact

* The library must export the UniFFI glue. On Windows:

  ```text
  dumpbin /exports target\release\core_ffi.dll | findstr uniffi
  ```

  On Android: `llvm-nm -D --defined-only libcore_ffi.so | grep uniffi`.

* The generated bindings verify a version and per-function checksums at
  initialization (`UniffiContractVersionException` /
  `UniffiContractChecksumException`). A load failure on Android almost
  always means the `.so` is missing for that ABI, or `libcore_ffi.so` was
  renamed.

* `PANIC: allocator` or a `SIGABRT` with `panic = "abort"` in the release
  profile terminates the process; the foreign side sees the app die, not an
  exception. `libcore_ffi` itself never panics: errors are values
  (`FfiError`) on every fallible path.

## Version pinning

UniFFI's metadata format is tied to the toolkit version:

* `uniffi = "0.31"` (resolves to 0.31.x) in `Cargo.toml`;
* `uniffi-bindgen-cs` tag `v0.11.1+v0.31.0`, which embeds the 0.31 bindings
  generator.

Bump both together, regenerate both languages, and re-run the tests. The
`cli` feature exists so the in-tree generator always matches the library;
it is not part of the shipping build.
