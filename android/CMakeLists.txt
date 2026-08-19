cmake_minimum_required(VERSION 3.22)
project(terminal_mirror CXX)

# The native core builds twice: on a developer host (Linux/macOS, plain g++/clang++)
# for unit, fuzz and integration tests, and for Android via the Gradle
# externalNativeBuild. Nothing in core/ may assume the Android NDK is present.

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
  set(CMAKE_BUILD_TYPE Debug CACHE STRING "" FORCE)
endif()

option(TM_BUILD_TESTS "Build the host test suite" ON)
option(TM_BUILD_FUZZERS "Build fuzz drivers" ON)
option(TM_ENABLE_GLES "Build the OpenGL ES renderer backend" OFF)
option(TM_ENABLE_ASAN "Build with address and undefined-behaviour sanitizers" OFF)

# spec §7.4: TLS validation is never bypassed in release builds. The escape hatch for
# tests is a debug-only *additional trust anchor*, not a disabled verifier; see
# core/src/net/tls.cpp.
if(ANDROID)
  set(TM_BUILD_TESTS OFF)
  set(TM_BUILD_FUZZERS OFF)
  set(TM_ENABLE_GLES ON)
endif()

add_compile_options(-Wall -Wextra -Wpedantic -Wshadow -Wconversion -Wsign-conversion
                    -Wno-unused-parameter)

if(TM_ENABLE_ASAN)
  add_compile_options(-fsanitize=address,undefined -fno-omit-frame-pointer)
  add_link_options(-fsanitize=address,undefined)
endif()

add_subdirectory(core)

if(TM_BUILD_TESTS)
  enable_testing()
  add_subdirectory(tests)
endif()
