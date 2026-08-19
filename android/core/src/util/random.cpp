#include "tm/util/random.h"

#include <openssl/rand.h>

namespace tmirror {

bool SecureRandomBytes(std::uint8_t* out, std::size_t size) {
  if (size == 0) return true;
  return RAND_bytes(out, static_cast<int>(size)) == 1;
}

Bytes SecureRandomBytes(std::size_t size) {
  Bytes out(size);
  if (!SecureRandomBytes(out.data(), out.size())) out.clear();
  return out;
}

std::uint64_t Prng::Next() {
  // splitmix64: small, fast and adequate for jitter and test corpora.
  state_ += 0x9E3779B97F4A7C15ULL;
  std::uint64_t z = state_;
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
  return z ^ (z >> 31);
}

std::uint64_t Prng::Below(std::uint64_t bound) {
  if (bound == 0) return 0;
  return Next() % bound;
}

double Prng::NextDouble() {
  return static_cast<double>(Next() >> 11) / static_cast<double>(1ULL << 53);
}

}  // namespace tmirror
