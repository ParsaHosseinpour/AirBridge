import java.io.File

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.plugin.compose")
}

android {
    namespace = "com.airbridge.app"
    compileSdk = 36
    compileSdkMinor = 1

    defaultConfig {
        applicationId = "com.airbridge.app"
        minSdk = 26
        targetSdk = 36
        versionCode = 1
        versionName = "1.0.0"

        vectorDrawables {
            useSupportLibrary = true
        }
    }

    buildTypes {
        debug {
            applicationIdSuffix = ".debug"
            versionNameSuffix = "-debug"
        }
        release {
            isMinifyEnabled = true
            isShrinkResources = true
            proguardFiles(
                getDefaultProguardFile("proguard-android-optimize.txt"),
                "proguard-rules.pro",
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
        buildConfig = true
    }

    packaging {
        // JNA expects to load its native dispatch library from the extracted
        // native library directory. Keeping legacy packaging on is the safest
        // option across the OEM range this app targets.
        jniLibs {
            useLegacyPackaging = true
        }
        resources {
            excludes += "/META-INF/{AL2.0,LGPL2.1}"
        }
    }

    lint {
        abortOnError = false
        checkReleaseBuilds = false
    }
}

dependencies {
    // The AAR module packages com.airbridge.core (generated UniFFI bindings,
    // Flow adapters, JNA linkage and the per-ABI libcore_ffi.so files).
    implementation(project(":libcore-ffi"))

    // Declared explicitly as well: the generated bindings use JNA to resolve
    // libcore_ffi.so, and the Android AAR variant ships libjnidispatch.
    implementation("net.java.dev.jna:jna:5.19.1@aar")

    val composeBom = platform("androidx.compose:compose-bom:2026.02.01")
    implementation(composeBom)
    androidTestImplementation(composeBom)

    implementation("androidx.core:core-ktx:1.18.0")
    implementation("androidx.activity:activity-compose:1.13.0")
    implementation("androidx.activity:activity-ktx:1.13.0")
    implementation("androidx.lifecycle:lifecycle-runtime-ktx:2.10.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.10.0")
    implementation("androidx.lifecycle:lifecycle-viewmodel-ktx:2.10.0")

    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-graphics")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.foundation:foundation")
    implementation("androidx.compose.animation:animation")
    implementation("androidx.compose.material3:material3")

    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0")

    testImplementation("junit:junit:4.13.2")

    debugImplementation("androidx.compose.ui:ui-tooling")
    debugImplementation("androidx.compose.ui:ui-test-manifest")
}

// Make missing native artifacts visible in the build log instead of letting the
// app die with UnsatisfiedLinkError on first launch. The screen also explains
// the fix at runtime.
val repoRoot: File = projectDir.resolve("../..")
val jniLibsDir: File = repoRoot.resolve("android/libcore-ffi/src/main/jniLibs")
val checkNativeLibraries by tasks.registering {
    description = "Warns when the UniFFI shared libraries have not been staged."
    doFirst {
        val libs = jniLibsDir.walkTopDown().filter { it.name == "libcore_ffi.so" }.toList()
        if (libs.isEmpty()) {
            logger.warn(
                "WARNING: no libcore_ffi.so under ${jniLibsDir.absolutePath}. " +
                    "The app will build, but discovery/transfers need the native core. " +
                    "Run `pwsh scripts/build-android.ps1 -NoAar` (or the .sh equivalent) first."
            )
        }
    }
}

tasks.named("preBuild") {
    dependsOn(checkNativeLibraries)
}
