// Root project of the Android build.
//
// Versions are deliberately explicit. AGP 9 provides built-in Kotlin support,
// so no `org.jetbrains.kotlin.android` plugin is applied; only the Compose
// compiler plugin is needed by the app module.
plugins {
    id("com.android.application") version "9.2.1" apply false
    id("com.android.library") version "9.2.1" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.2.10" apply false
}
