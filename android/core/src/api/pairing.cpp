#include "tm/api/pairing.h"

#include "tm/util/base64.h"
#include "tm/util/json.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace api {
namespace {

constexpr const char kPrefix[] = "HT1.";
constexpr std::size_t kPrefixLength = 4;
/// A pairing code carries a URL, an identity fingerprint and a token. Well under this;
/// the bound exists so a pasted file cannot be parsed as one.
constexpr std::size_t kMaxCodeBytes = 8192;

}  // namespace

std::string EncodePairingCode(const PairingCode& code) {
  Json object = Json::Object();
  // Short keys: this is typed or pasted by a person, so every character counts.
  object.Set("u", Json::String(code.server_url));
  object.Set("i", Json::String(code.identity_id));
  object.Set("t", Json::String(code.identity_token));
  const std::string json = object.Serialize();
  return std::string(kPrefix) + Base64UrlEncode(ByteView(json));
}

Result<PairingCode> DecodePairingCode(const std::string& text) {
  const std::string trimmed = Trim(text);
  if (trimmed.size() > kMaxCodeBytes) {
    return Status::Error(ErrorKind::kInvalidArgument, "that pairing code is too long");
  }
  if (trimmed.size() <= kPrefixLength || trimmed.compare(0, kPrefixLength, kPrefix) != 0) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "that does not look like a pairing code; it starts with HT1.");
  }

  Bytes decoded;
  if (!Base64UrlDecode(trimmed.substr(kPrefixLength), &decoded)) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "the pairing code is damaged; copy it again");
  }

  JsonLimits limits;
  limits.max_bytes = kMaxCodeBytes;
  limits.max_depth = 4;
  limits.max_elements = 32;
  Result<Json> parsed =
      Json::Parse(std::string(decoded.begin(), decoded.end()), limits);
  if (!parsed.ok() || !parsed.value().is_object()) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "the pairing code is damaged; copy it again");
  }

  PairingCode code;
  code.server_url = parsed.value().GetString("u");
  code.identity_id = parsed.value().GetString("i");
  code.identity_token = parsed.value().GetString("t");
  if (code.server_url.empty() || code.identity_id.empty() || code.identity_token.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "the pairing code is missing part of itself; generate a new one");
  }
  return code;
}

}  // namespace api
}  // namespace tmirror
