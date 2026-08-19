#pragma once

#include <string>

#include "tm/util/bytes.h"
#include "tm/util/result.h"

namespace tmirror {
namespace api {

/// Everything a device needs to join an identity, in one string (relay spec §5.2).
///
/// Registering a device takes two parties: the request must be authorised by the
/// *owner*, and the challenge must be signed by the *device*, so neither side can
/// enrol a key alone. The phone holds its own key and cannot borrow the identity's,
/// so what crosses is a short-lived identity token — the owner's half of the
/// exchange, delegated for a few minutes.
///
/// That makes the code a credential while it lives. It is deliberately short-lived,
/// single-purpose, and never written to disk on the device: only the resulting
/// device credential is kept.
struct PairingCode {
  std::string server_url;
  std::string identity_id;
  /// A `devices:write` identity token. Valid for minutes, not days.
  std::string identity_token;

  /// The token is a bearer credential for as long as it lives, so it is wiped rather
  /// than left in freed memory — the same treatment the device key seed gets.
  ~PairingCode() { SecureZero(identity_token); }
  PairingCode() = default;
  PairingCode(PairingCode&&) = default;
  PairingCode& operator=(PairingCode&&) = default;
  PairingCode(const PairingCode&) = delete;
  PairingCode& operator=(const PairingCode&) = delete;
};

/// `HT1.<base64url(json)>`.
///
/// The prefix is there so a truncated or half-pasted code fails immediately with
/// something a user can act on, rather than as a signature rejection several requests
/// later.
std::string EncodePairingCode(const PairingCode& code);

Result<PairingCode> DecodePairingCode(const std::string& text);

}  // namespace api
}  // namespace tmirror
