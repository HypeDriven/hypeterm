#include "tm/crypto/identity.h"

#include "tm/crypto/crypto.h"
#include "tm/util/base64.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace crypto {

const char kIdentityFingerprintContext[] = "terminal-relay-identity-v1";
const char kChallengeSignatureContext[] = "terminal-relay-challenge-v1";
const char kAlgorithmEd25519[] = "ed25519";

LengthPrefixed& LengthPrefixed::Field(ByteView bytes) {
  std::uint32_t length = static_cast<std::uint32_t>(bytes.size());
  buffer_.push_back(static_cast<std::uint8_t>((length >> 24) & 0xFF));
  buffer_.push_back(static_cast<std::uint8_t>((length >> 16) & 0xFF));
  buffer_.push_back(static_cast<std::uint8_t>((length >> 8) & 0xFF));
  buffer_.push_back(static_cast<std::uint8_t>(length & 0xFF));
  buffer_.insert(buffer_.end(), bytes.begin(), bytes.end());
  return *this;
}

LengthPrefixed& LengthPrefixed::FieldString(const std::string& s) {
  return Field(ByteView(s));
}

LengthPrefixed& LengthPrefixed::FieldUint64(std::uint64_t value) {
  std::uint8_t encoded[8];
  for (int i = 0; i < 8; ++i) {
    encoded[i] = static_cast<std::uint8_t>((value >> (56 - 8 * i)) & 0xFF);
  }
  return Field(ByteView(encoded, sizeof(encoded)));
}

bool LengthPrefixed::Split(ByteView encoded, std::vector<Bytes>* fields) {
  fields->clear();
  std::size_t position = 0;
  while (position < encoded.size()) {
    if (encoded.size() - position < 4) return false;
    std::uint32_t length = (static_cast<std::uint32_t>(encoded[position]) << 24) |
                           (static_cast<std::uint32_t>(encoded[position + 1]) << 16) |
                           (static_cast<std::uint32_t>(encoded[position + 2]) << 8) |
                           static_cast<std::uint32_t>(encoded[position + 3]);
    position += 4;
    if (encoded.size() - position < length) return false;
    fields->emplace_back(encoded.data() + position, encoded.data() + position + length);
    position += length;
    if (fields->size() > 64) return false;  // bounded: this is untrusted input
  }
  return true;
}

std::string KeyFingerprint(const std::string& algorithm, ByteView public_key) {
  LengthPrefixed encoder;
  encoder.FieldString(kIdentityFingerprintContext)
      .FieldString(ToLowerAscii(algorithm))
      .Field(public_key);
  return Base64UrlEncode(ByteView(Sha256(ByteView(encoder.Finish()))));
}

const char* ChallengeOperationName(ChallengeOperation operation) {
  switch (operation) {
    case ChallengeOperation::kRegisterIdentity: return "register_identity";
    case ChallengeOperation::kAuthenticateIdentity: return "authenticate_identity";
    case ChallengeOperation::kRegisterDevice: return "register_device";
    case ChallengeOperation::kAuthenticateDevice: return "authenticate_device";
  }
  return "";
}

bool ParseChallengeOperation(const std::string& name, ChallengeOperation* out) {
  if (name == "register_identity") {
    *out = ChallengeOperation::kRegisterIdentity;
  } else if (name == "authenticate_identity") {
    *out = ChallengeOperation::kAuthenticateIdentity;
  } else if (name == "register_device") {
    *out = ChallengeOperation::kRegisterDevice;
  } else if (name == "authenticate_device") {
    *out = ChallengeOperation::kAuthenticateDevice;
  } else {
    return false;
  }
  return true;
}

Bytes SigningInput::Encode() const {
  LengthPrefixed encoder;
  encoder.FieldString(kChallengeSignatureContext)
      .FieldString(origin)
      .FieldString(challenge_id)
      .Field(ByteView(challenge))
      .FieldString(ChallengeOperationName(operation))
      .FieldString(key_fingerprint)
      .FieldString(owner_identity_id)
      .FieldString(device_key_fingerprint)
      .FieldUint64(expires_at_unix_ms);
  return encoder.Finish();
}

Status VerifySigningInput(ByteView signing_input, const ExpectedSigningInput& expected) {
  std::vector<Bytes> fields;
  if (!LengthPrefixed::Split(signing_input, &fields) || fields.size() != 9) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input is not the expected nine-field encoding");
  }
  auto as_string = [&](std::size_t index) { return StringFromBytes(fields[index]); };

  if (as_string(0) != kChallengeSignatureContext) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input has an unexpected context");
  }
  if (!expected.expected_origin.empty() && as_string(1) != expected.expected_origin) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds an unexpected origin");
  }
  if (as_string(2) != expected.challenge_id) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds a different challenge id");
  }
  if (fields[3] != expected.challenge) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds different challenge bytes");
  }
  if (as_string(4) != ChallengeOperationName(expected.operation)) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds a different operation");
  }
  if (as_string(5) != expected.key_fingerprint) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds a different key");
  }
  if (!expected.owner_identity_id.empty() && as_string(6) != expected.owner_identity_id) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds a different owner identity");
  }
  if (!expected.device_key_fingerprint.empty() && as_string(7) != expected.device_key_fingerprint) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input binds a different device key");
  }
  if (fields[8].size() != 8) {
    return Status::Error(ErrorKind::kProtocolError,
                         "challenge signing input has a malformed expiry field");
  }
  return Status::Ok();
}

}  // namespace crypto
}  // namespace tmirror
