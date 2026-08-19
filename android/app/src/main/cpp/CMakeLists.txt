cmake_minimum_required(VERSION 3.22)
project(terminal_mirror_jni CXX)

# The Gradle externalNativeBuild entry point. It compiles the shared core from
# ../../../../core and adds only the JNI bridge on top: no terminal, protocol or
# rendering logic lives in this directory (spec §6.1).

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

set(TM_CORE_ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../../../../core)

set(TM_ENABLE_GLES ON CACHE BOOL "" FORCE)
set(TM_BUILD_TESTS OFF CACHE BOOL "" FORCE)
set(TM_BUILD_FUZZERS OFF CACHE BOOL "" FORCE)

# OpenSSL is a static build produced by tools/build-openssl-android.sh; Gradle passes
# its location as TM_OPENSSL_ROOT and core/CMakeLists.txt picks the per-ABI directory.
add_subdirectory(${TM_CORE_ROOT} ${CMAKE_CURRENT_BINARY_DIR}/core)

add_library(hypeterm SHARED
  jni_bridge.cpp
  android_rasterizer.cpp
  render_thread.cpp
)

target_include_directories(hypeterm PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(hypeterm PRIVATE tm_core android log EGL GLESv3)
target_compile_options(hypeterm PRIVATE -Wall -Wextra -Wno-unused-parameter)

# Android 15 and later require native libraries to load on devices with 16 KB pages.
# NDK r28 does this by default; r27 needs to be told.
target_link_options(hypeterm PRIVATE
  "-Wl,-z,max-page-size=16384" "-Wl,-z,common-page-size=16384")
