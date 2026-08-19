#pragma once

#include <map>
#include <memory>
#include <mutex>
#include <string>

#include "tm/crypto/crypto.h"
#include "tm/util/bytes.h"
#include "tm/util/result.h"

namespace tmirror {
namespace api {

/// Key-value storage whose implementation is responsible for encryption at rest.
///
/// On Android this is backed by the Keystore (spec §12): the host layer holds an
/// AES-GCM key that never leaves the secure hardware and uses it to seal each value.
/// The core deliberately does not know how that works — it only knows that values it
/// stores here are protected and that it must not write them anywhere else.
class SecureStore {
 public:
  virtual ~SecureStore() = default;
  virtual Status Put(const std::string& key, ByteView value) = 0;
  virtual Result<Bytes> Get(const std::string& key) = 0;
  virtual Status Remove(const std::string& key) = 0;
  virtual bool Contains(const std::string& key) = 0;
};

/// Process-lifetime store used by tests and by the pairing screen before anything is
/// persisted. Never used for production credentials.
class InMemorySecureStore : public SecureStore {
 public:
  Status Put(const std::string& key, ByteView value) override;
  Result<Bytes> Get(const std::string& key) override;
  Status Remove(const std::string& key) override;
  bool Contains(const std::string& key) override;

 private:
  std::mutex mutex_;
  std::map<std::string, Bytes> values_;
};

/// The client's own credential: a `client`-role device key (relay reconciliation
/// §1.2). The identity's root private key never reaches the device.
struct DeviceCredentials {
  std::string server_url;
  std::string identity_id;
  std::string device_id;
  std::string device_name;
  /// Ed25519 seed. Held only as long as needed and zeroed on destruction.
  Bytes private_key_seed;
  std::string public_key_base64url;
  std::string key_fingerprint;

  ~DeviceCredentials() { SecureZero(private_key_seed); }
  DeviceCredentials() = default;
  DeviceCredentials(DeviceCredentials&&) = default;
  DeviceCredentials& operator=(DeviceCredentials&&) = default;
  DeviceCredentials(const DeviceCredentials&) = delete;
  DeviceCredentials& operator=(const DeviceCredentials&) = delete;

  bool complete() const {
    return !server_url.empty() && !device_id.empty() && private_key_seed.size() == 32;
  }
  Result<crypto::Ed25519KeyPair> LoadKeyPair() const;
};

/// Reads and writes the device credential through a SecureStore.
class CredentialStore {
 public:
  explicit CredentialStore(SecureStore* store) : store_(store) {}

  /// Generates a fresh key pair. The caller then displays the public key for pairing
  /// (relay reconciliation §2.2) and saves the credential once the owner has
  /// registered it.
  static Result<DeviceCredentials> GenerateNew(const std::string& server_url,
                                               const std::string& device_name);

  Result<DeviceCredentials> Load();
  Status Save(const DeviceCredentials& credentials);
  Status Clear();
  bool HasCredentials();

 private:
  SecureStore* store_;
};

}  // namespace api
}  // namespace tmirror
