#include "tm/api/credentials.h"

#include "tm/crypto/identity.h"
#include "tm/util/base64.h"
#include "tm/util/json.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace api {
namespace {

constexpr const char kCredentialKey[] = "device_credentials_v1";

}  // namespace

Status InMemorySecureStore::Put(const std::string& key, ByteView value) {
  std::lock_guard<std::mutex> lock(mutex_);
  values_[key] = value.to_bytes();
  return Status::Ok();
}

Result<Bytes> InMemorySecureStore::Get(const std::string& key) {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = values_.find(key);
  if (it == values_.end()) {
    return Status::Error(ErrorKind::kNotFound, "no such stored value");
  }
  return it->second;
}

Status InMemorySecureStore::Remove(const std::string& key) {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = values_.find(key);
  if (it != values_.end()) {
    SecureZero(it->second);
    values_.erase(it);
  }
  return Status::Ok();
}

bool InMemorySecureStore::Contains(const std::string& key) {
  std::lock_guard<std::mutex> lock(mutex_);
  return values_.find(key) != values_.end();
}

Result<crypto::Ed25519KeyPair> DeviceCredentials::LoadKeyPair() const {
  return crypto::Ed25519KeyPair::FromSeed(ByteView(private_key_seed));
}

Result<DeviceCredentials> CredentialStore::GenerateNew(const std::string& server_url,
                                                       const std::string& device_name) {
  Result<crypto::Ed25519KeyPair> key = crypto::Ed25519KeyPair::Generate();
  if (!key.ok()) return key.status();

  DeviceCredentials credentials;
  credentials.server_url = server_url;
  credentials.device_name = device_name;
  credentials.private_key_seed = key.value().seed();
  credentials.public_key_base64url = Base64UrlEncode(ByteView(key.value().public_key()));
  credentials.key_fingerprint =
      crypto::KeyFingerprint(crypto::kAlgorithmEd25519, ByteView(key.value().public_key()));
  return credentials;
}

Status CredentialStore::Save(const DeviceCredentials& credentials) {
  if (credentials.private_key_seed.size() != crypto::Ed25519KeyPair::kSeedSize) {
    return Status::Error(ErrorKind::kInvalidArgument, "credential has no usable private key");
  }
  Json object = Json::Object();
  object.Set("server_url", Json::String(credentials.server_url));
  object.Set("identity_id", Json::String(credentials.identity_id));
  object.Set("device_id", Json::String(credentials.device_id));
  object.Set("device_name", Json::String(credentials.device_name));
  object.Set("public_key", Json::String(credentials.public_key_base64url));
  object.Set("key_fingerprint", Json::String(credentials.key_fingerprint));
  object.Set("private_key_seed",
             Json::String(Base64UrlEncode(ByteView(credentials.private_key_seed))));

  std::string serialized = object.Serialize();
  Status status = store_->Put(kCredentialKey, ByteView(serialized));
  // The serialized form held the seed; clear it before the buffer is reused.
  SecureZero(serialized);
  return status;
}

Result<DeviceCredentials> CredentialStore::Load() {
  Result<Bytes> stored = store_->Get(kCredentialKey);
  if (!stored.ok()) return stored.status();

  std::string text = StringFromBytes(stored.value());
  Result<Json> parsed = Json::Parse(text);
  SecureZero(stored.value());
  if (!parsed.ok()) {
    SecureZero(text);
    return Status::Error(ErrorKind::kStorageError, "stored credential is unreadable");
  }

  DeviceCredentials credentials;
  credentials.server_url = parsed.value().GetString("server_url");
  credentials.identity_id = parsed.value().GetString("identity_id");
  credentials.device_id = parsed.value().GetString("device_id");
  credentials.device_name = parsed.value().GetString("device_name");
  credentials.public_key_base64url = parsed.value().GetString("public_key");
  credentials.key_fingerprint = parsed.value().GetString("key_fingerprint");
  bool decoded =
      Base64UrlDecode(parsed.value().GetString("private_key_seed"), &credentials.private_key_seed);
  SecureZero(text);
  if (!decoded || credentials.private_key_seed.size() != crypto::Ed25519KeyPair::kSeedSize) {
    return Status::Error(ErrorKind::kStorageError, "stored credential has no usable key");
  }
  return credentials;
}

Status CredentialStore::Clear() { return store_->Remove(kCredentialKey); }

bool CredentialStore::HasCredentials() { return store_->Contains(kCredentialKey); }

}  // namespace api
}  // namespace tmirror
