plugins {
    id("com.android.application") version "8.3.1"
}

val probeAbi: String by project

android {
    namespace = "dev.slipmat.musickitprobe"
    compileSdk = 34

    defaultConfig {
        applicationId = "dev.slipmat.musickitprobe"
        minSdk = 23
        targetSdk = 34
        ndk {
            abiFilters += probeAbi
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs.useLegacyPackaging = true
    }
}

dependencies {
    implementation(files("libs/MediaPlayback-release-1.1.4.aar"))
    implementation(files("libs/MusicKitAuth-release-1.1.5.aar"))
    implementation("androidx.appcompat:appcompat:1.7.0")
    implementation("androidx.media:media:1.7.0")
    implementation("org.jetbrains.kotlin:kotlin-stdlib-jdk7:1.9.23")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.6.0")
}
