#include "tm/util/time.h"

namespace tmirror {
namespace {

class SystemClock : public Clock {
 public:
  Millis MonotonicMillis() override {
    auto now = std::chrono::steady_clock::now().time_since_epoch();
    return std::chrono::duration_cast<std::chrono::milliseconds>(now).count();
  }
  Millis UnixMillis() override {
    auto now = std::chrono::system_clock::now().time_since_epoch();
    return std::chrono::duration_cast<std::chrono::milliseconds>(now).count();
  }
};

}  // namespace

Clock* Clock::System() {
  static SystemClock clock;
  return &clock;
}

}  // namespace tmirror
