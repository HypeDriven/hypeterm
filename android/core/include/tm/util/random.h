#pragma once

#include <cstdint>

#include "tm/util/bytes.h"

namespace tmirror {

/// Cryptographically secure bytes: private keys, WebSocket masks, nonces.
bool SecureRandomBytes(std::uint8_t* out, std::size_t size);
Bytes SecureRandomBytes(std::size_t size);

/// Deterministic, seedable generator used for reconnect jitter and for fuzz corpora.
/// Not for key material.
class Prng {
 public:
  explicit Prng(std::uint64_t seed = 0x9E3779B97F4A7C15ULL) : state_(seed | 1u) {}
  std::uint64_t Next();
  /// Uniform in [0, bound); returns 0 when bound is 0.
  std::uint64_t Below(std::uint64_t bound);
  double NextDouble();

 private:
  std::uint64_t state_;
};

}  // namespace tmirror
