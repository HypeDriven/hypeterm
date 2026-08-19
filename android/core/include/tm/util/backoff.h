#pragma once

#include <cstdint>

#include "tm/util/random.h"
#include "tm/util/time.h"

namespace tmirror {

/// Exponential reconnect backoff with jitter, reset after a stable connection
/// (spec §7.4, §11).
///
/// "Stable" is a duration, not merely a successful handshake: a connection that dies
/// immediately after connecting must not reset the delay, or a server that accepts and
/// instantly drops connections would be hammered at the base interval forever.
class Backoff {
 public:
  struct Options {
    Millis initial_delay_ms = 500;
    Millis max_delay_ms = 30000;
    double multiplier = 2.0;
    /// Fraction of the computed delay that is randomised, in [0, 1].
    double jitter = 0.3;
    /// How long a connection must survive before the sequence resets.
    Millis stability_threshold_ms = 10000;
  };

  Backoff() : Backoff(Options()) {}
  explicit Backoff(const Options& options, std::uint64_t seed = 0x5DEECE66DULL)
      : options_(options), prng_(seed) {}

  /// Delay before the next attempt, and advance the sequence.
  Millis NextDelay();
  /// Called when a connection has been established; records the time so that
  /// `RecordDisconnected` can judge stability.
  void RecordConnected(Millis monotonic_now);
  /// Called when a connection ends. Resets the sequence when the connection lasted
  /// at least the stability threshold.
  void RecordDisconnected(Millis monotonic_now);
  void Reset();

  unsigned attempt() const { return attempt_; }

 private:
  Options options_;
  Prng prng_;
  unsigned attempt_ = 0;
  Millis connected_at_ = -1;
};

}  // namespace tmirror
