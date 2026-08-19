#pragma once

#include <functional>
#include <string>

#include "tm/util/bytes.h"

namespace tmirror {

enum class LogLevel { kVerbose = 0, kDebug, kInfo, kWarn, kError, kOff };

/// Logging is deliberately awkward to misuse.
///
/// Spec §9.3, §12, §15 and acceptance criterion 8 forbid terminal input, terminal
/// output, keystrokes, tokens, tickets, challenges and signatures from appearing in
/// release logs. Two mechanisms enforce that here:
///
///  * `TM_LOG_PAYLOAD` compiles to nothing outside debug builds, so a payload log
///    line cannot exist in a release binary even if someone writes one.
///  * `Redacted()` and `ByteCount()` produce the only representations of sensitive
///    values that any build may log.
class Log {
 public:
  using Sink = std::function<void(LogLevel, const std::string& tag, const std::string& message)>;

  static void SetSink(Sink sink);
  static void SetLevel(LogLevel level);
  static LogLevel level();
  static bool Enabled(LogLevel level);
  static void Write(LogLevel level, const std::string& tag, const std::string& message);

  /// Fixed-length marker: reveals that a secret was present, never its content.
  static std::string Redacted() { return "<redacted>"; }
  /// The only permitted description of a payload in any build.
  static std::string ByteCount(std::size_t bytes);
};

void LogFormat(LogLevel level, const char* tag, const char* format, ...)
#if defined(__GNUC__)
    __attribute__((format(printf, 3, 4)))
#endif
    ;

}  // namespace tmirror

#define TM_LOG_ERROR(tag, ...) ::tmirror::LogFormat(::tmirror::LogLevel::kError, tag, __VA_ARGS__)
#define TM_LOG_WARN(tag, ...) ::tmirror::LogFormat(::tmirror::LogLevel::kWarn, tag, __VA_ARGS__)
#define TM_LOG_INFO(tag, ...) ::tmirror::LogFormat(::tmirror::LogLevel::kInfo, tag, __VA_ARGS__)
#define TM_LOG_DEBUG(tag, ...) ::tmirror::LogFormat(::tmirror::LogLevel::kDebug, tag, __VA_ARGS__)

#if defined(TM_DEBUG_BUILD)
// Debug-only, and even here the caller is expected to have redacted first.
#define TM_LOG_PAYLOAD(tag, ...) ::tmirror::LogFormat(::tmirror::LogLevel::kVerbose, tag, __VA_ARGS__)
#else
#define TM_LOG_PAYLOAD(tag, ...) ((void)0)
#endif
