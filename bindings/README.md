# Bindings

This directory holds the **hand-written** adapters over the generated UniFFI
surface. The generated code itself is written to `target/bindings/` and is not
committed.

```text
bindings/
  kotlin/
    src/com/airbridge/core/Events.kt   PeerDiscoveryEngine.events(), FileTransferSession.events()
  csharp/
    AirBridge.Core.csproj              compiles hand-written + generated sources into one assembly
    PeerDiscoveryEvents.cs             .NET event pump over the discovery stream
    FileTransferEvents.cs              .NET event pump over the transfer stream

target/bindings/kotlin/   (generated, ignored)
target/bindings/csharp/   (generated, ignored)
```

The task-oriented documentation for the crate lives in
`crates/libcore_ffi/README.md`; this file only covers how the adapters are
consumed and regenerated.

## Regenerating

The generator reads API metadata out of a compiled library, so build first.
Both scripts do the whole dance:

```powershell
pwsh scripts/build-windows.ps1      # host library + C# bindings
pwsh scripts/build-android.ps1      # NDK libraries + Kotlin bindings + AAR
```

The underlying commands, if you prefer to run them by hand:

```text
cargo run -p libcore_ffi --features cli --bin uniffi-bindgen -- \
    generate --library target/release/core_ffi.dll \
    --language kotlin --out-dir target/bindings/kotlin \
    --config crates/libcore_ffi/uniffi.toml --no-format

uniffi-bindgen-cs --library target/release/core_ffi.dll \
    --out-dir target/bindings/csharp \
    --config crates/libcore_ffi/uniffi.toml --no-format
```

Never edit generated files; edit the Rust API and regenerate. The unit of
versioning is the pair (uniffi, uniffi-bindgen-cs) — see the crate README,
"Version pinning".

## Kotlin

The canonical consumer is the Gradle module in `android/`, which adds
`bindings/kotlin/src` and `target/bindings/kotlin` as source directories and
produces an AAR. To embed the bindings in an existing app instead, add both
directories the same way and depend on:

```kotlin
dependencies {
    implementation("net.java.dev.jna:jna:5.14.0@aar")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
}
```

The generated `libcore_ffi.so` files must be packaged per ABI (`jniLibs/`).
The Android module targets minSdk 24 for all four ABIs:
`arm64-v8a`, `armeabi-v7a`, `x86_64`, `x86`.

Usage:

```kotlin
val engine = PeerDiscoveryEngine(DiscoveryConfig())

lifecycleScope.launch {
    engine.start(backend)                 // BleBackend? — null is valid
    engine.events()                       // from bindings/kotlin/.../Events.kt
        .onEach { event -> handle(event) }
        .collect()                        // completes after Shutdown
}
```

A `BleBackend` implementation is an ordinary Kotlin object; the interface is
generated. Return by throwing `FfiException.InvalidArgument("…")` (or any
other `FfiException` variant) to report a failure — it becomes the `Err` the
coordinator sees; any other exception is reported as an unexpected internal
error.

```kotlin
class PlatformBle(private val context: Context) : BleBackend {
    private var sink: BleObservationSink? = null

    override fun start(request: BleRequest, sink: BleObservationSink) {
        if (context.checkSelfPermission(BLUETOOTH_ADVERTISE) != PERMISSION_GRANTED) {
            throw FfiException.InvalidArgument("missing BLUETOOTH_ADVERTISE")
        }
        this.sink = sink
        // Start advertiser + scanner; forward sightings with
        // sink.observe(idHex, port) from the radio callbacks.
    }

    override fun stop() { sink = null /* stop radio */ }

    override fun isRunning(): Boolean = /* radio attached? */ true
}
```

Remember: `start`/`stop`/`isRunning` are called on Rust runtime threads, and
`observe` may be called from the radio's thread. Keep all four non-blocking
and thread-safe (see the crate README, "Threading and JNA (Android)").

## C#

Compile the wrapper assembly once, then reference it from the app:

```powershell
dotnet build bindings\csharp\AirBridge.Core.csproj
```

`AirBridge.Core.dll` contains the generated interop plus the two event pumps.
Copy `core_ffi.dll` next to the app's binaries (or anywhere on the DLL search
path); the bindings resolve the bare name `core_ffi`.

The generated code targets modern C#; for .NET Framework 4.6.1 add
`LangVersion=10` and the `PolySharp` package as `uniffi-bindgen-cs`
documents. The project here targets `net8.0`.

Usage:

```csharp
var engine = new PeerDiscoveryEngine(new DiscoveryConfig());
var events = new PeerDiscoveryEvents(engine);
events.EventReceived += (_, e) => Console.WriteLine(e);
events.Closed         += (_, _) => Console.WriteLine("discovery ended");
events.Start();
engine.Start(bleBackend: null);

// Later, from any thread:
await events.DisposeAsync();   // stops the engine and drains the pump
```

A `BleBackend` implementation works the same way; the generated interface is
public. Throw `FfiException` (or a nested variant such as
`FfiException.InvalidArgument`) to report a failure to the coordinator.

```csharp
internal sealed class WindowsBle : BleBackend
{
    private BleObservationSink? _sink;

    public void Start(BleRequest request, BleObservationSink sink)
    {
        _sink = sink;
        // Start BluetoothLEAdvertisementPublisher/Watcher, forward sightings:
        // sink.Observe(idHex, port);
    }

    public void Stop() => _sink = null;

    public bool IsRunning() => true;
}
```

For a transfer, drive `Run()` and the pump concurrently:

```csharp
var session = FileTransferSession.Send(address, filePath, new SendOptions());
await using var events = new FileTransferEvents(session);
events.EventReceived += (_, e) => { if (e is TransferEvent.Progress p) Show(p); };
events.Start();

TransferReport report = await session.Run();
await events.DisposeAsync();     // pump already ended after the terminal event
```
