#include "tm/util/backoff.h"

#include <algorithm>
#include <cmath>

namespace tmirror {

Millis Backoff::NextDelay() {
  double delay = static_cast<double>(options_.initial_delay_ms) *
                 std::pow(options_.multiplier, static_cast<double>(attempt_));
  if (delay > static_cast<double>(options_.max_delay_ms)) {
    delay = static_cast<double>(options_.max_delay_ms);
  }
  if (attempt_ < 32) ++attempt_;

  double jitter = options_.jitter;
  if (jitter < 0.0) jitter = 0.0;
  if (jitter > 1.0) jitter = 1.0;
  // Randomise downward only, so the delay never exceeds the configured maximum.
  double factor = 1.0 - jitter * prng_.NextDouble();
  double jittered = delay * factor;
  Millis result = static_cast<Millis>(jittered);
  return result < 0 ? 0 : result;
}

void Backoff::RecordConnected(Millis monotonic_now) { connected_at_ = monotonic_now; }

void Backoff::RecordDisconnected(Millis monotonic_now) {
  if (connected_at_ >= 0 && monotonic_now - connected_at_ >= options_.stability_threshold_ms) {
    Reset();
  }
  connected_at_ = -1;
}

void Backoff::Reset() {
  attempt_ = 0;
  connected_at_ = -1;
}

}  // namespace tmirror
