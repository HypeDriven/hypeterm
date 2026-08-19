#include "tm/util/log.h"

#include <atomic>
#include <cstdarg>
#include <cstdio>
#include <mutex>

#include "tm/util/strings.h"

namespace tmirror {
namespace {

std::mutex& SinkMutex() {
  static std::mutex m;
  return m;
}

Log::Sink& SinkRef() {
  static Log::Sink sink;
  return sink;
}

std::atomic<int>& LevelRef() {
  static std::atomic<int> level(
#if defined(TM_DEBUG_BUILD)
      static_cast<int>(LogLevel::kDebug)
#else
      static_cast<int>(LogLevel::kInfo)
#endif
  );
  return level;
}

const char* LevelName(LogLevel level) {
  switch (level) {
    case LogLevel::kVerbose: return "V";
    case LogLevel::kDebug: return "D";
    case LogLevel::kInfo: return "I";
    case LogLevel::kWarn: return "W";
    case LogLevel::kError: return "E";
    case LogLevel::kOff: return "-";
  }
  return "?";
}

}  // namespace

void Log::SetSink(Sink sink) {
  std::lock_guard<std::mutex> lock(SinkMutex());
  SinkRef() = std::move(sink);
}

void Log::SetLevel(LogLevel level) { LevelRef().store(static_cast<int>(level)); }

LogLevel Log::level() { return static_cast<LogLevel>(LevelRef().load()); }

bool Log::Enabled(LogLevel level) {
  return static_cast<int>(level) >= LevelRef().load() &&
         static_cast<int>(level) < static_cast<int>(LogLevel::kOff);
}

void Log::Write(LogLevel level, const std::string& tag, const std::string& message) {
  if (!Enabled(level)) return;
  Sink sink;
  {
    std::lock_guard<std::mutex> lock(SinkMutex());
    sink = SinkRef();
  }
  if (sink) {
    sink(level, tag, message);
  } else {
    std::fprintf(stderr, "%s/%s: %s\n", LevelName(level), tag.c_str(), message.c_str());
  }
}

std::string Log::ByteCount(std::size_t bytes) {
  return Uint64ToString(static_cast<std::uint64_t>(bytes)) + " bytes";
}

void LogFormat(LogLevel level, const char* tag, const char* format, ...) {
  if (!Log::Enabled(level)) return;
  char buffer[1024];
  va_list args;
  va_start(args, format);
  int written = std::vsnprintf(buffer, sizeof(buffer), format, args);
  va_end(args);
  if (written < 0) return;
  std::size_t length = static_cast<std::size_t>(written);
  if (length >= sizeof(buffer)) length = sizeof(buffer) - 1;
  Log::Write(level, tag, std::string(buffer, length));
}

}  // namespace tmirror
