#pragma once

#include <chrono>
#include <cstdint>
#include <memory>

namespace tmirror {

using Millis = std::int64_t;

/// Injectable clock. Reconnect backoff, heartbeats, debounce windows and token expiry
/// are all time-driven, and every one of them has a test that would otherwise have to
/// sleep.
class Clock {
 public:
  virtual ~Clock() = default;
  /// Monotonic milliseconds since an arbitrary epoch; never moves backwards.
  virtual Millis MonotonicMillis() = 0;
  /// Wall-clock milliseconds since the Unix epoch, for token expiry comparisons.
  virtual Millis UnixMillis() = 0;

  static Clock* System();
};

class ManualClock : public Clock {
 public:
  Millis MonotonicMillis() override { return monotonic_; }
  Millis UnixMillis() override { return unix_; }
  void Advance(Millis delta) {
    monotonic_ += delta;
    unix_ += delta;
  }
  void SetUnixMillis(Millis value) { unix_ = value; }

 private:
  Millis monotonic_ = 1000;
  Millis unix_ = 1700000000000;
};

}  // namespace tmirror
