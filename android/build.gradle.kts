// Root build file. The application module is :app; the native core it compiles lives
// in core/ and is shared verbatim with the host test build (spec §6.1).
//
// Versions are pinned to what the local SDK provides: platform 35, NDK 27, and the
// AGP/Kotlin releases already in the Gradle cache.
plugins {
    id("com.android.application") version "8.7.3" apply false
    kotlin("android") version "2.1.0" apply false
}
