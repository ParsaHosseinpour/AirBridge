package com.airbridge.app.core

import com.airbridge.core.uniffiEnsureInitialized

data class NativeCoreStatus(
    val available: Boolean,
    val message: String? = null,
)

/**
 * Checks that the staged `libcore_ffi.so` and JNA are usable before any
 * discovery or transfer object is constructed. A failed load is reported in
 * the UI instead of crashing the process with [UnsatisfiedLinkError].
 */
object NativeCore {
    private val lock = Any()
    private var checked = false
    private var cached = NativeCoreStatus(available = false)

    fun status(): NativeCoreStatus = synchronized(lock) {
        if (!checked) {
            cached = try {
                uniffiEnsureInitialized()
                NativeCoreStatus(available = true)
            } catch (t: Throwable) {
                NativeCoreStatus(
                    available = false,
                    message = buildString {
                        append("The native core could not be loaded on this device")
                        val detail = t.message?.takeIf { it.isNotBlank() }
                        if (detail != null) append(": ").append(detail)
                        append(". Build libcore_ffi.so for this ABI and reinstall the app.")
                    },
                )
            }
            checked = true
        }
        cached
    }
}
