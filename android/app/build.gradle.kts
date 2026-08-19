import java.util.Properties

plugins {
    id("com.android.application")
    kotlin("android")
}

// Where the prebuilt Tailscale node lives, if it was built at all. `-PtsnetRoot=…`
// overrides `tsnet.dir` in local.properties, which overrides the default cache path
// the build script writes to.
val tsnetRoot: String? = run {
    val fromProperty = project.findProperty("tsnetRoot") as String?
    val fromLocal = Properties().let { properties ->
        val file = rootProject.file("local.properties")
        if (file.exists()) file.inputStream().use { properties.load(it) }
        properties.getProperty("tsnet.dir")
    }
    val candidate = fromProperty
        ?: fromLocal
        ?: "${System.getProperty("user.home")}/.cache/hypeterm/tsnet-android"
    if (file(candidate).isDirectory) candidate else null
}

// Where the static OpenSSL build lives. `-Ptm.openssl.root=…` overrides `openssl.dir`
// in local.properties, which overrides the default cache path tools/build-openssl-android.sh
// writes to. Machine-specific paths therefore stay out of the committed build files.
val opensslRoot: String = run {
    val fromProperty = project.findProperty("tm.openssl.root") as String?
    val fromLocal = Properties().let { properties ->
        val file = rootProject.file("local.properties")
        if (file.exists()) file.inputStream().use { properties.load(it) }
        properties.getProperty("openssl.dir")
    }
    fromProperty
        ?: fromLocal
        ?: "${System.getProperty("user.home")}/.cache/hypeterm/openssl-android"
}

// Release signing. The keystore is deliberately outside the tree and its location and
// password come from local.properties, which is not committed — same arrangement as the
// OpenSSL and tsnet paths above. A clone without it still builds; only assembleRelease
// produces an unsigned artifact.
val releaseSigning: Map<String, String>? = run {
    val properties = Properties().also { properties ->
        val file = rootProject.file("local.properties")
        if (file.exists()) file.inputStream().use { properties.load(it) }
    }
    val store = properties.getProperty("release.keystore")
    val alias = properties.getProperty("release.alias")
    val password = properties.getProperty("release.password")
    if (store != null && alias != null && password != null && file(store).isFile) {
        mapOf("store" to store, "alias" to alias, "password" to password)
    } else {
        null
    }
}

android {
    namespace = "com.hypedriven.hypeterm"
    compileSdk = 35

    defaultConfig {
        applicationId = "com.hypedriven.hypeterm"
        // Android 10 (spec: target platform is API 29 or later).
        minSdk = 29
        targetSdk = 35
        versionCode = 1
        versionName = "0.1"

        ndk {
            // 64-bit only: 32-bit ABIs are no longer required for new applications and
            // every device at API 29+ that matters supports one of these.
            abiFilters += listOf("arm64-v8a", "x86_64")
        }

        externalNativeBuild {
            cmake {
                arguments += listOf(
                    // One shared library ships, so the static STL is safe and keeps
                    // libc++_shared.so out of the APK entirely.
                    "-DANDROID_STL=c++_static",
                    "-DTM_OPENSSL_ROOT=$opensslRoot",
                    // Android 15+ devices may use 16 KB pages.
                    "-DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON",
                )
                cppFlags += listOf("-std=c++17")
            }
        }
    }

    externalNativeBuild {
        cmake {
            path = file("src/main/cpp/CMakeLists.txt")
            version = "3.22.1"
        }
    }

    signingConfigs {
        releaseSigning?.let { signing ->
            create("release") {
                storeFile = file(signing.getValue("store"))
                storePassword = signing.getValue("password")
                keyAlias = signing.getValue("alias")
                keyPassword = signing.getValue("password")
            }
        }
    }

    buildTypes {
        debug {
            isMinifyEnabled = false
            // Debug builds may log redacted diagnostics; release builds compile the
            // payload logging out entirely (spec §9.3, §15).
            isJniDebuggable = true
        }
        release {
            releaseSigning?.let { signingConfig = signingConfigs.getByName("release") }
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

    kotlinOptions {
        jvmTarget = "17"
    }

    sourceSets {
        getByName("main") {
            java.srcDirs("src/main/kotlin")
            // The embedded Tailscale node, built by tools/build-tsnet-android.sh into
            // <root>/<abi>/libhypeterm_tsnet.so. It is optional: when the directory is
            // absent the APK simply ships without it, dlopen fails, and the app reports
            // that the tunnel is not included rather than falling back to a direct
            // connection (core/src/net/tailscale_dialer.cpp).
            tsnetRoot?.let { jniLibs.srcDirs(it) }
        }
    }

    packaging {
        jniLibs {
            useLegacyPackaging = false
        }
    }
}

dependencies {
    // Deliberately none. The client's dependencies are the platform and the native
    // core; adding a networking or terminal library here would put protocol or terminal
    // behaviour on the JVM side, which spec §6.1 places in C++. OpenSSL is linked
    // statically into the native library — see tools/build-openssl-android.sh.
}
