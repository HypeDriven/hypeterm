#pragma once

#include <string>
#include <vector>

#include "tm/util/bytes.h"
#include "tm/util/result.h"

namespace tmirror {
namespace crypto {

/// Unambiguous length-prefixed concatenation: each field is preceded by its length as
/// an unsigned 32-bit network-byte-order value (relay spec §3.1, §4.2).
class LengthPrefixed {
 public:
  LengthPrefixed& Field(ByteView bytes);
  LengthPrefixed& FieldString(const std::string& s);
  LengthPrefixed& FieldUint64(std::uint64_t value);
  const Bytes& Finish() const { return buffer_; }

  /// Split an encoding back into its fields. Returns false on any inconsistency,
  /// which matters because the client re-derives what it is about to sign rather
  /// than trusting a server-supplied blob.
  static bool Split(ByteView encoded, std::vector<Bytes>* fields);

 private:
  Bytes buffer_;
};

extern const char kIdentityFingerprintContext[];  // "terminal-relay-identity-v1"
extern const char kChallengeSignatureContext[];   // "terminal-relay-challenge-v1"
extern const char kAlgorithmEd25519[];            // "ed25519"

/// base64url(SHA-256(lp(context) || lp(algorithm) || lp(public key))), unpadded.
/// For an identity this is also its canonical ID (relay spec §3.1).
std::string KeyFingerprint(const std::string& algorithm, ByteView public_key);

enum class ChallengeOperation {
  kRegisterIdentity,
  kAuthenticateIdentity,
  kRegisterDevice,
  kAuthenticateDevice,
};

const char* ChallengeOperationName(ChallengeOperation operation);
bool ParseChallengeOperation(const std::string& name, ChallengeOperation* out);

/// Everything the relay binds into a proof-of-possession signature (relay spec §4.2).
/// Fields that do not apply to an operation are present but empty, so no field
/// boundary is ambiguous.
struct SigningInput {
  std::string origin;
  std::string challenge_id;
  Bytes challenge;
  ChallengeOperation operation = ChallengeOperation::kAuthenticateDevice;
  std::string key_fingerprint;
  std::string owner_identity_id;
  std::string device_key_fingerprint;
  std::uint64_t expires_at_unix_ms = 0;

  Bytes Encode() const;
};

/// What the client believes it is signing. `VerifySigningInput` checks a server-
/// supplied `signing_input` against these before the key is ever used, so a malicious
/// or buggy relay cannot obtain a signature over bytes of its choosing.
struct ExpectedSigningInput {
  std::string challenge_id;
  Bytes challenge;
  ChallengeOperation operation = ChallengeOperation::kAuthenticateDevice;
  std::string key_fingerprint;
  /// Checked only when non-empty (device registration binds the owner identity).
  std::string owner_identity_id;
  std::string device_key_fingerprint;
  /// Checked only when non-empty; the relay's public origin may legitimately differ
  /// from the URL the user typed (a proxy, an alternate host name).
  std::string expected_origin;
};

Status VerifySigningInput(ByteView signing_input, const ExpectedSigningInput& expected);

}  // namespace crypto
}  // namespace tmirror
