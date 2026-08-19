#pragma once

#include <memory>
#include <string>

#include "tm/util/bytes.h"
#include "tm/util/result.h"

namespace tmirror {
namespace crypto {

Bytes Sha256(ByteView data);

/// Only used for the WebSocket handshake accept value (RFC 6455 §4.1), where the
/// algorithm is fixed by the protocol and carries no security requirement.
Bytes Sha1(ByteView data);

/// Constant-time comparison for anything secret-derived.
bool ConstantTimeEquals(ByteView a, ByteView b);

/// Ed25519 signing key. The seed is the 32-byte private scalar seed that the Android
/// host layer seals with a Keystore-backed AES key (spec §12); it never leaves the
/// device and is zeroed on destruction.
class Ed25519KeyPair {
 public:
  static constexpr std::size_t kSeedSize = 32;
  static constexpr std::size_t kPublicKeySize = 32;
  static constexpr std::size_t kSignatureSize = 64;

  Ed25519KeyPair() = default;
  ~Ed25519KeyPair();
  Ed25519KeyPair(Ed25519KeyPair&&) noexcept;
  Ed25519KeyPair& operator=(Ed25519KeyPair&&) noexcept;
  Ed25519KeyPair(const Ed25519KeyPair&) = delete;
  Ed25519KeyPair& operator=(const Ed25519KeyPair&) = delete;

  static Result<Ed25519KeyPair> Generate();
  static Result<Ed25519KeyPair> FromSeed(ByteView seed);

  bool valid() const { return !seed_.empty(); }
  const Bytes& seed() const { return seed_; }
  const Bytes& public_key() const { return public_key_; }

  Result<Bytes> Sign(ByteView message) const;

 private:
  Bytes seed_;
  Bytes public_key_;
};

bool Ed25519Verify(ByteView public_key, ByteView message, ByteView signature);

}  // namespace crypto
}  // namespace tmirror
