# The native core (spec §6.1). Terminal emulation, protocol translation, networking
# orchestration and rendering all live here; the JVM layer only bridges platform APIs.

set(TM_CORE_SOURCES
  src/util/base64.cpp
  src/util/json.cpp
  src/util/log.cpp
  src/util/backoff.cpp
  src/util/random.cpp
  src/util/time.cpp
  src/util/strings.cpp

  src/crypto/crypto.cpp
  src/crypto/identity.cpp

  src/term/utf8.cpp
  src/term/width.cpp
  src/term/screen.cpp
  src/term/scrollback.cpp
  src/term/parser.cpp
  src/term/emulator.cpp

  src/input/key_encoder.cpp
  src/input/paste.cpp
  src/input/typed_text.cpp

  src/net/url.cpp
  src/net/socket.cpp
  src/net/tls.cpp
  src/net/http_client.cpp
  src/net/websocket.cpp
  src/net/tailscale_dialer.cpp

  src/api/relay_client.cpp
  src/api/mirror_session.cpp
  src/api/credentials.cpp
  src/api/pairing.cpp

  src/render/metrics.cpp
  src/render/atlas.cpp
  src/render/frame_builder.cpp
  src/render/view.cpp
  src/render/follow.cpp
  src/render/reference_renderer.cpp

  src/app/controller.cpp
  src/app/session.cpp
  src/app/persistence.cpp
  src/app/config.cpp
)

if(TM_ENABLE_GLES)
  list(APPEND TM_CORE_SOURCES src/render/gl_renderer.cpp)
endif()

add_library(tm_core STATIC ${TM_CORE_SOURCES})

target_include_directories(tm_core PUBLIC ${CMAKE_CURRENT_SOURCE_DIR}/include)

# OpenSSL provides TLS, SHA-2 and Ed25519.
#
# On a developer machine it is the system package. On Android the NDK ships no crypto
# library, so it is built by tools/build-openssl-android.sh and linked *statically*:
# that keeps the APK to one native library, and it is the only way to get 16 KB-aligned
# objects, which Android 15+ requires and the prebuilt 1.1.1 packages do not have.
if(ANDROID)
  if(NOT TM_OPENSSL_ROOT)
    message(FATAL_ERROR
      "TM_OPENSSL_ROOT is not set. Run tools/build-openssl-android.sh, which builds "
      "into the directory the Gradle build looks in by default "
      "(see docs/android-build.md).")
  endif()
  set(TM_OPENSSL_ABI_ROOT "${TM_OPENSSL_ROOT}/${ANDROID_ABI}")
  if(NOT EXISTS "${TM_OPENSSL_ABI_ROOT}/lib/libcrypto.a")
    message(FATAL_ERROR "No OpenSSL build for ${ANDROID_ABI} at ${TM_OPENSSL_ABI_ROOT}")
  endif()
  target_include_directories(tm_core PUBLIC "${TM_OPENSSL_ABI_ROOT}/include")
  # ssl before crypto: the static link order matters.
  target_link_libraries(tm_core PUBLIC
    "${TM_OPENSSL_ABI_ROOT}/lib/libssl.a"
    "${TM_OPENSSL_ABI_ROOT}/lib/libcrypto.a")
else()
  find_package(OpenSSL REQUIRED)
  target_link_libraries(tm_core PUBLIC OpenSSL::SSL OpenSSL::Crypto)
endif()

find_package(Threads REQUIRED)
target_link_libraries(tm_core PUBLIC Threads::Threads)

# The embedded Tailscale node is loaded with dlopen so a build without it still runs
# (core/src/net/tailscale_dialer.cpp). Bionic and modern glibc have dlopen in libc;
# CMAKE_DL_LIBS covers the platforms that still need -ldl.
target_link_libraries(tm_core PUBLIC ${CMAKE_DL_LIBS})

if(TM_ENABLE_GLES)
  target_compile_definitions(tm_core PUBLIC TM_ENABLE_GLES=1)
  if(ANDROID)
    target_link_libraries(tm_core PUBLIC GLESv3 EGL android log)
  endif()
endif()

# Release builds must not contain payload logging at all (spec §9.3, §12, §15).
target_compile_definitions(tm_core PUBLIC
  $<$<CONFIG:Debug>:TM_DEBUG_BUILD=1>
  $<$<NOT:$<CONFIG:Debug>>:TM_RELEASE_BUILD=1>)
