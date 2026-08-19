#pragma once

#include <memory>
#include <string>

#include "tm/api/events.h"
#include "tm/crypto/crypto.h"
#include "tm/crypto/identity.h"
#include "tm/net/http_client.h"
#include "tm/net/url.h"
#include "tm/util/json.h"

namespace tmirror {
namespace api {

struct RelayClientConfig {
  net::Url base_url;
  net::TlsConfig tls;
  Millis connect_timeout_ms = 15000;
  Millis request_timeout_ms = 30000;
  std::string user_agent = "TerminalMirror/0.1";
  /// Optional tunnel; see net::TransportOptions.
  net::Dialer* dialer = nullptr;
  bool allow_cleartext_over_tunnel = false;
};

/// The HTTP half of the API adapter (spec §7.1).
///
/// Everything protocol-specific about the relay's REST surface lives here so that a
/// server change cannot reach the emulator or the renderer (spec §18 closing line).
class RelayClient {
 public:
  explicit RelayClient(RelayClientConfig config);

  const RelayClientConfig& config() const { return config_; }
  void set_cancel_signal(std::shared_ptr<net::CancelSignal> cancel) {
    cancel_ = std::move(cancel);
  }

  // ---------------------------------------------------------------- primitives

  Result<Challenge> CreateChallenge(crypto::ChallengeOperation operation,
                                    const std::string& algorithm, ByteView public_key,
                                    const std::string& owner_identity_id = std::string());

  /// `POST /v1/identities`. Idempotent for a key that is already registered.
  Result<std::string> RegisterIdentity(const std::string& challenge_id, ByteView signature);

  Result<AccessToken> CreateToken(const std::string& challenge_id, ByteView signature);

  /// Single-use, path-bound ticket for a WebSocket upgrade (relay spec §5.1). Native
  /// clients normally use `Authorization` instead; this exists for parity and for
  /// deployments that strip the header at a proxy.
  Result<std::string> CreateWebSocketTicket(const AccessToken& token, const std::string& path);

  Result<DeviceInfo> RegisterDevice(const AccessToken& identity_token, const std::string& name,
                                    const std::string& algorithm, ByteView device_public_key,
                                    const std::string& challenge_id, ByteView device_signature,
                                    const std::string& role);

  Result<std::vector<DeviceInfo>> ListDevices(const AccessToken& token);
  Status RevokeDevice(const AccessToken& token, const std::string& device_id);

  Result<TerminalPage> ListTerminals(const AccessToken& token, const std::string& state_filter,
                                     const std::string& cursor, int limit);
  Result<TerminalInfo> GetTerminal(const AccessToken& token, const std::string& terminal_id);

  /// Asks a device's publisher to open a terminal (relay spec §4.6, §5.2).
  ///
  /// The request deliberately carries only a label and a geometry: the machine at the
  /// far end decides what runs, and a relay that accepted a command here would be
  /// letting a phone choose. `idempotency_key` is required by the endpoint, because
  /// this makes a process exist and a retry must not make a second one — reuse the
  /// same key when retrying, which is how an ambiguous timeout is resolved.
  ///
  /// Blocking: it waits for the far machine to answer. Call it off the network thread.
  Result<TerminalInfo> OpenTerminal(const AccessToken& token, const std::string& device_id,
                                    const std::string& label, int columns, int rows,
                                    const std::string& idempotency_key);

  // ------------------------------------------------------------------- flows

  /// Full proof-of-possession authentication for a key that is already registered as
  /// an identity. There are no refresh tokens: re-authentication *is* the refresh
  /// (relay reconciliation §2.2).
  Result<AccessToken> AuthenticateIdentity(const crypto::Ed25519KeyPair& key);

  /// Same, for a `client`-role device key held in Keystore-backed storage.
  Result<AccessToken> AuthenticateDevice(const crypto::Ed25519KeyPair& key);

  /// Register a new identity for a freshly generated key.
  Result<std::string> RegisterIdentityForKey(const crypto::Ed25519KeyPair& key);

  /// Pairing, run on the machine that holds the identity key: it obtains a
  /// `register_device` challenge bound to the owner, has the *device* sign it, and
  /// registers the device. The phone's private key never leaves the phone; only the
  /// signature and public key cross.
  Result<Challenge> CreateDeviceRegistrationChallenge(const std::string& algorithm,
                                                      ByteView device_public_key,
                                                      const std::string& owner_identity_id);

 private:
  Result<net::HttpResponse> Send(const std::string& method, const std::string& target,
                                 const std::string& body, const AccessToken* token);
  Result<Json> SendJson(const std::string& method, const std::string& target,
                        const std::string& body, const AccessToken* token);
  Result<AccessToken> AuthenticateWithOperation(const crypto::Ed25519KeyPair& key,
                                                crypto::ChallengeOperation operation);

  RelayClientConfig config_;
  net::HttpClientConfig http_config_;
  std::shared_ptr<net::CancelSignal> cancel_;
};

/// Maps an HTTP status plus the relay's error envelope onto a Status (spec §15).
Status StatusFromHttp(int http_status, const std::string& body);

}  // namespace api
}  // namespace tmirror
