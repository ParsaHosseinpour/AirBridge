import java.io.File

plugins {
    id("com.android.library")
}

// The generated UniFFI bindings and the hand-written flow adapters live
// outside this Gradle module so that one copy serves every consumer. Both
// directories are wired in as source roots below; scripts/build-android.*
// populates the generated one.
val repoRoot: File = projectDir.resolve("../..")
val kotlinAdaptersDir: File = repoRoot.resolve("bindings/kotlin/src")
val kotlinGeneratedDir: File = repoRoot.resolve("target/bindings/kotlin")
val generatedBindings: File = kotlinGeneratedDir.resolve("com/airbridge/core/core_ffi.kt")

android {
    namespace = "com.airbridge.core"
    compileSdk = 36
    compileSdkMinor = 1

    defaultConfig {
        minSdk = 24
        consumerProguardFiles("consumer-rules.pro")
    }

    sourceSets {
        getByName("main") {
            // With AGP 9's built-in Kotlin support, additional Kotlin source
            // roots must be registered on the Kotlin directory set.
            kotlin.directories += listOf(
                kotlinAdaptersDir.absolutePath,
                kotlinGeneratedDir.absolutePath,
            )
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildTypes {
        getByName("release") {
            isMinifyEnabled = false
        }
    }
}

dependencies {
    // JNA loads libcore_ffi.so and drives the JNI shims. The generated code
    // references it directly, but exposes none of its types, so it stays an
    // implementation detail.
    implementation("net.java.dev.jna:jna:5.19.1@aar")
    // The generated suspend functions and the hand-written flow adapters put
    // `Flow` and `suspend` in the AAR's public API.
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.9.0")
}

// UniFFI 0.31's Kotlin backend emits a `message` constructor property next to
// an override of Throwable.message in every FfiException subclass. Modern
// Kotlin compilers reject that combination ("hides member ... needs an
// 'override' modifier"), so normalise the generated file in place after
// bindgen writes it. The transform is idempotent and also lives in
// scripts/build-android.* for builds that generate bindings outside Gradle.
val patchKotlinBindings by tasks.registering {
    description = "Adapts the generated Kotlin bindings to modern Kotlin error rules."
    val bindingsFile = generatedBindings
    outputs.file(bindingsFile)
    doLast {
        if (!bindingsFile.isFile) return@doLast
        var text = bindingsFile.readText()
        val before = text

        val subclassParameter = Regex(
            "(?<!override )val `message`: kotlin\\.String(\\s*\\)\\s*:\\s*FfiException\\(\\))"
        )
        text = text.replace(subclassParameter) { match -> "override " + match.value }

        val redundantOverride = Regex(
            "\\s*override val message\\s*\\r?\\n\\s*get\\(\\) = \"message=\\$\\{ `message` \\}\""
        )
        text = text.replace(redundantOverride, "")

        if (text != before) {
            bindingsFile.writeText(text)
            logger.lifecycle("Patched ${bindingsFile.name} for Kotlin 2.2+ error-property rules.")
        }
    }
}

// Fail with instructions instead of a wall of unresolved references when the
// bindings have not been generated yet.
val checkGeneratedBindings by tasks.registering {
    description = "Verifies that the UniFFI Kotlin bindings have been generated."
    doFirst {
        if (!generatedBindings.isFile) {
            throw GradleException(
                "Kotlin bindings not found at ${generatedBindings.absolutePath}. " +
                    "Run scripts/build-android.ps1 (or .sh) first."
            )
        }
    }
}

tasks.named("preBuild") {
    dependsOn(checkGeneratedBindings)
    dependsOn(patchKotlinBindings)
}
