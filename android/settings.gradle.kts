// Android build for AirBridge.
//
// `:libcore-ffi` packages the hand-written Kotlin adapters
// (bindings/kotlin/src) and the generated UniFFI bindings
// (target/bindings/kotlin) together with the staged `.so` files. `:app` is the
// Compose frontend and consumes the library as a normal project dependency.

pluginManagement {
    repositories {
        google()
        mavenCentral()
        gradlePluginPortal()
    }
}

dependencyResolutionManagement {
    repositoriesMode.set(RepositoriesMode.FAIL_ON_PROJECT_REPOS)
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "airbridge-android"

include(":libcore-ffi")
include(":app")
