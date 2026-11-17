plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "com.mimint.shell"
    compileSdk = 34

    defaultConfig {
        applicationId = "com.mimint.shell"
        // minSdk 26: AndroidKeyStore self-signed certificate
        // generation (KeyGenParameterSpec certificate fields)
        // and the notification-channel APIs the foreground
        // service needs both require API 26+.
        minSdk = 26
        targetSdk = 34
        versionCode = 1
        versionName = "0.1.0-phase4"
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            // No signing config ships here: the developer signs
            // with their own key. Never commit a keystore.
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
    // Intentionally empty: the shell uses only the Android
    // platform (org.json, javax.net.ssl, AndroidKeyStore).
    // No third-party code runs with accessibility privileges.
}
