// Pairing against a relay that checks signatures (relay spec §5.2).
//
// Registering a device takes both parties: the owner authorises the request, and the
// device proves it holds the key by signing a challenge bound to that owner. The
// client's original flow only did the first half — it displayed a public key and
// recorded whatever IDs came back — which worked against the fake relay's test-only
// shortcut and cannot work against the real one. These cover the flow that does.

#include <memory>
#include <string>

#include "harness.h"
#include "tm/api/credentials.h"
#include "tm/api/pairing.h"
#include "tm/app/controller.h"
#include "tm/app/persistence.h"
#include "tm/util/base64.h"

using tmirror::ErrorKind;
using tmirror::Result;
using tmirror::api::CredentialStore;
using tmirror::api::DecodePairingCode;
using tmirror::api::EncodePairingCode;
using tmirror::api::InMemorySecureStore;
using tmirror::api::PairingCode;
using tmirror::app::AppConfig;
using tmirror::app::Controller;
using tmirror::app::ControllerCallbacks;
using tmirror::app::PairingInfo;
using tmirror::app::Preferences;
using tmtest::FakeRelay;

namespace {

struct Client {
  InMemorySecureStore store;
  Preferences preferences{"/tmp/tm_pairing_prefs.json"};
  std::unique_ptr<Controller> controller;

  explicit Client(const std::string& server_url) {
    AppConfig config;
    config.server_url = server_url;
    config.device_name = "test phone";
    config.connect_timeout_ms = 5000;
    controller = std::make_unique<Controller>(config, &store, &preferences,
                                              ControllerCallbacks());
  }
};

}  // namespace

TM_TEST(Pairing, ACodeSurvivesEncodingAndDecoding) {
  PairingCode code;
  code.server_url = "https://hypeterm-relay.example.ts.net";
  code.identity_id = "LQxkHHvuDL8L6NGEezI_65Rr4DkfsLGN1BqO-RYCDTs";
  code.identity_token = "v1.abc.def";

  const std::string encoded = EncodePairingCode(code);
  // The prefix is what makes a half-pasted code fail immediately and legibly.
  TM_CHECK_EQ(encoded.compare(0, 4, "HT1."), 0);

  Result<PairingCode> decoded = DecodePairingCode(encoded);
  TM_REQUIRE(decoded.ok());
  TM_CHECK_EQ(decoded.value().server_url, code.server_url);
  TM_CHECK_EQ(decoded.value().identity_id, code.identity_id);
  TM_CHECK_EQ(decoded.value().identity_token, code.identity_token);
}

TM_TEST(Pairing, TheEncodingAgreesWithThePublisher) {
  // The exact string `hypeterm-publish pair-code` produces. The Rust side asserts the
  // same literal in publisher/src/pairing.rs, so neither implementation can change the
  // encoding without the other's test failing — which is the only thing keeping two
  // programs in two languages speaking the same format.
  const std::string vector =
      "HT1.eyJ1IjoiaHR0cHM6Ly9yZWxheS5leGFtcGxlIiwiaSI6ImlkZW50aXR5LWZpbmdlcnByaW50Iiwi"
      "dCI6InYxLnBheWxvYWQudGFnIn0";

  Result<PairingCode> decoded = DecodePairingCode(vector);
  TM_REQUIRE(decoded.ok());
  TM_CHECK_EQ(decoded.value().server_url, "https://relay.example");
  TM_CHECK_EQ(decoded.value().identity_id, "identity-fingerprint");
  TM_CHECK_EQ(decoded.value().identity_token, "v1.payload.tag");
}

TM_TEST(Pairing, MangledCodesAreRefusedWithSomethingToActOn) {
  PairingCode code;
  code.server_url = "https://relay.example";
  code.identity_id = "identity";
  code.identity_token = "token";
  const std::string encoded = EncodePairingCode(code);

  TM_CHECK(!DecodePairingCode("").ok());
  TM_CHECK(!DecodePairingCode("hello").ok());
  // Truncated: the common failure, from a code that wrapped in a chat window.
  TM_CHECK(!DecodePairingCode(encoded.substr(0, encoded.size() - 5)).ok());
  // Right shape, missing a field: a code from an older or broken producer.
  TM_CHECK(!DecodePairingCode("HT1." + tmirror::Base64UrlEncode(tmirror::ByteView(
                                           std::string("{\"u\":\"https://x\"}"))))
                .ok());

  // Surrounding whitespace is what a paste brings with it, and is not the user's
  // fault.
  Result<PairingCode> padded = DecodePairingCode("  " + encoded + "\n");
  TM_CHECK(padded.ok());
}

TM_TEST(Pairing, ADeviceEnrolsItselfWithACodeAndThenAttaches) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Pairing: the fake relay could not be started\n");
    return;
  }
  Result<FakeRelay::Owner> owner = relay.CreateOwnerIdentity();
  TM_REQUIRE(owner.ok());

  PairingCode code;
  code.server_url = relay.base_url();
  code.identity_id = owner.value().identity_id;
  code.identity_token = owner.value().identity_token;

  Client client(relay.base_url());
  // The device generates its own key and shows it; the private half never leaves.
  Result<PairingInfo> info = client.controller->BeginPairing();
  TM_REQUIRE(info.ok());
  TM_CHECK(!info.value().public_key_base64url.empty());
  TM_CHECK(!client.controller->HasCredentials());

  Result<std::string> paired =
      client.controller->CompletePairingWithCode(EncodePairingCode(code));
  TM_CHECK_MSG(paired.ok(), paired.status().message());
  TM_REQUIRE(paired.ok());
  TM_CHECK(client.controller->HasCredentials());
  // The code carries the relay's address, so pairing is also how the client learns
  // where to connect.
  TM_CHECK_EQ(paired.value(), relay.base_url());

  // The credential must be usable, not merely stored: authenticate and list.
  TM_REQUIRE(client.controller->Start().ok());
  const std::string terminal_id = relay.CreateTerminal("paired shell", 40, 10, true);
  TM_REQUIRE(!terminal_id.empty());
  client.controller->Attach(terminal_id);
  TM_CHECK(tmtest::WaitFor(
      [&] {
        return client.controller->status().state ==
               tmirror::app::ConnectionState::kAttached;
      },
      10000));
  client.controller->Stop();
}

TM_TEST(Pairing, ACodeForAnotherIdentityIsRefusedBeforeSigning) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Pairing: the fake relay could not be started\n");
    return;
  }
  Result<FakeRelay::Owner> owner = relay.CreateOwnerIdentity();
  TM_REQUIRE(owner.ok());

  // A code whose token belongs to one identity but which names another. The relay
  // would reject it eventually; the point is that this device refuses to put its
  // signature on a statement binding it to an identity it was not given (spec §12).
  PairingCode code;
  code.server_url = relay.base_url();
  code.identity_id = "not-the-identity-in-the-token";
  code.identity_token = owner.value().identity_token;

  Client client(relay.base_url());
  TM_REQUIRE(client.controller->BeginPairing().ok());
  Result<std::string> paired =
      client.controller->CompletePairingWithCode(EncodePairingCode(code));
  TM_CHECK(!paired.ok());
  TM_CHECK(!client.controller->HasCredentials());
}

TM_TEST(Pairing, ACodeNamingACleartextRelayIsRefused) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Pairing: the fake relay could not be started\n");
    return;
  }
  Result<FakeRelay::Owner> owner = relay.CreateOwnerIdentity();
  TM_REQUIRE(owner.ok());

  // A pairing code is pasted in by a person and could have come from anywhere. One
  // naming a plain-HTTP relay on some other host must not cause this device to send a
  // registration — and the credential it would enrol — in the clear (spec §7.4).
  PairingCode code;
  code.server_url = "http://relay.example.invalid:8080";
  code.identity_id = owner.value().identity_id;
  code.identity_token = owner.value().identity_token;

  Client client(relay.base_url());
  TM_REQUIRE(client.controller->BeginPairing().ok());
  Result<std::string> paired =
      client.controller->CompletePairingWithCode(EncodePairingCode(code));
  TM_CHECK(!paired.ok());
  TM_CHECK(paired.status().kind() == ErrorKind::kTlsFailure);
  TM_CHECK(!client.controller->HasCredentials());
}
