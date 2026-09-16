plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "org.anazoa.vpn"
    compileSdk = 34

    defaultConfig {
        applicationId = "org.anazoa.vpn"
        // The Rust .so (livekit/webrtc-sys/libwebrtc/android-arm64-release,
        // and anazoa-tun's cdylib) was built with a floor of API 23, so 24
        // satisfies it; bumped from 23 because Service.STOP_FOREGROUND_REMOVE
        // (used in AnazoaVpnService) needs 24.
        minSdk = 24
        targetSdk = 34
        versionCode = 1
        versionName = "0.3.0"

        // libanazoa_tun.so under src/main/jniLibs/arm64-v8a is the only
        // artifact we currently build (aarch64-linux-android); restrict
        // packaging to match rather than silently shipping an APK that's
        // missing native libs on other ABIs.
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    kotlinOptions {
        jvmTarget = "17"
    }
}

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.activity:activity-ktx:1.9.3")

    // libwebrtc's own Android Java classes (livekit.org.webrtc.*,
    // livekit.org.jni_zero.*), package-relocated to the "livekit" prefix to
    // match the native side's android_package_prefix GN arg. Without these,
    // libwebrtc's native init calls FindClass for classes that don't exist
    // anywhere in the app's dex and ART aborts with
    // "JNI DETECTED ERROR IN APPLICATION: ... ClassNotFoundException".
    // Built from livekit/webrtc-sys/libwebrtc/prefixed-jni (its own gradlew
    // shadowJar task) — see android/rebuild.sh.
    implementation(files("libs/libwebrtc.jar"))
}
