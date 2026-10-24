import java.util.Properties

plugins {
    alias(libs.plugins.android.application)
    alias(libs.plugins.kotlin.android)
    alias(libs.plugins.kotlin.compose)
}

val repoRoot = rootDir.resolve("../..").canonicalFile
val workspaceVersion: String = Regex("""(?m)^version\s*=\s*"([^"]+)"""")
    .find(repoRoot.resolve("Cargo.toml").readText())?.groupValues?.get(1) ?: "0.0.0"

android {
    namespace = "dev.secondscrn.client"
    compileSdk = 36
    ndkVersion = "29.0.14206865"

    defaultConfig {
        applicationId = "dev.secondscrn.client"
        minSdk = 28
        targetSdk = 36
        versionName = workspaceVersion
        versionCode = workspaceVersion.split(".").fold(0) { acc, p -> acc * 1000 + (p.toIntOrNull() ?: 0) }
        // arm64 for real tablets, x86_64 for the emulator.
        ndk { abiFilters += listOf("arm64-v8a", "x86_64") }
    }

    signingConfigs {
        // Release signing is configured through a keystore.properties file
        // (not committed). Without it, release builds are unsigned.
        val props = rootDir.resolve("keystore.properties")
        if (props.exists()) {
            val p = Properties().apply { props.inputStream().use { load(it) } }
            create("release") {
                storeFile = file(p.getProperty("storeFile"))
                storePassword = p.getProperty("storePassword")
                keyAlias = p.getProperty("keyAlias")
                keyPassword = p.getProperty("keyPassword")
            }
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = true
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
            signingConfig = signingConfigs.findByName("release")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlin {
        compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) }
    }
    buildFeatures {
        compose = true
        buildConfig = true
    }
    packaging {
        jniLibs { useLegacyPackaging = false }
    }
}

// --- Rust core: build the native library and generate the UniFFI bindings.
// Always optimised: a debug build of the media path is too slow to be useful.
val cargoProfile = "release"
val jniLibsDir = projectDir.resolve("src/main/jniLibs")
val bindingsDir = projectDir.resolve("src/main/java")

val buildRust by tasks.registering(Exec::class) {
    group = "rust"
    description = "Builds libsecondscrn_client.so for all ABIs with cargo-ndk"
    workingDir = repoRoot
    inputs.dir(repoRoot.resolve("crates"))
    inputs.file(repoRoot.resolve("Cargo.lock"))
    outputs.dir(jniLibsDir)
    environment("ANDROID_NDK_HOME", android.ndkDirectory.absolutePath)
    commandLine(
        "cargo", "ndk", "-t", "arm64-v8a", "-t", "x86_64", "-P", "28", "-o", jniLibsDir.absolutePath,
        "build", "-p", "secondscrn-android-client", "--profile", cargoProfile,
    )
}

val generateBindings by tasks.registering(Exec::class) {
    group = "rust"
    description = "Generates Kotlin bindings from the compiled library (UniFFI library mode)"
    dependsOn(buildRust)
    workingDir = repoRoot
    inputs.dir(jniLibsDir)
    outputs.dir(bindingsDir.resolve("uniffi"))
    commandLine(
        "cargo", "run", "-q", "-p", "uniffi-bindgen", "--", "generate",
        "--library", jniLibsDir.resolve("x86_64/libsecondscrn_client.so").absolutePath,
        "--language", "kotlin", "--out-dir", bindingsDir.absolutePath, "--no-format",
    )
}

tasks.named("preBuild") { dependsOn(generateBindings) }

dependencies {
    implementation(libs.androidx.core.ktx)
    implementation(libs.androidx.activity.compose)
    implementation(libs.androidx.lifecycle.runtime.compose)
    implementation(libs.androidx.lifecycle.viewmodel.compose)
    implementation(platform(libs.androidx.compose.bom))
    implementation(libs.androidx.compose.ui)
    implementation(libs.androidx.compose.ui.tooling.preview)
    implementation(libs.androidx.compose.material3)
    implementation(libs.androidx.compose.material.icons)
    implementation(libs.kotlinx.coroutines.android)
    implementation("${libs.jna.get()}@aar")
    debugImplementation(libs.androidx.compose.ui.tooling)
}
