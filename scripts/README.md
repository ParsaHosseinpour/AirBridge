# Build scripts

| script                | what it does                                                        |
|-----------------------|---------------------------------------------------------------------|
| `build-windows.ps1`   | host `core_ffi.dll` + C# bindings (+ `-BuildWrapper` for the .NET assembly) |
| `build-windows.sh`    | the same on Linux/macOS (host `.so`/`.dylib` + C# bindings)          |
| `build-android.ps1`   | four-ABI `libcore_ffi.so` set + Kotlin bindings + AAR               |
| `build-android.sh`    | the same on Linux/macOS                                             |

The Windows scripts accept `-Toolchain stable-x86_64-pc-windows-gnu` for
machines without the MSVC linker (see the workspace README, "Toolchain notes
(Windows)"). The Android scripts accept `-Abis`, `-ApiLevel` and `-NoAar`.

Generated artifacts (all ignored by git):

```text
target/<profile>/core_ffi.dll          or libcore_ffi.so / libcore_ffi.dylib
target/bindings/csharp/core_ffi.cs
target/bindings/kotlin/com/airbridge/core/core_ffi.kt
android/libcore-ffi/src/main/jniLibs/<abi>/libcore_ffi.so
android/libcore-ffi/build/outputs/aar/libcore-ffi-release.aar
```

No script honours `CARGO_TARGET_DIR`; they expect the default in-repo `target/`
so the bindings generator and Gradle agree on where to look.
