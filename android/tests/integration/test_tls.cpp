// TLS behaviour end to end (spec §7.4, §15).
//
// The rest of the integration suite runs over loopback cleartext, which never exercises
// the verifier. These tests start the fake relay behind a real TLS listener with a
// self-signed certificate and check both halves of the requirement: a correctly
// anchored certificate connects, and an unanchored or mis-named one fails with a TLS
// error rather than silently succeeding.

#include <cstdio>
#include <cstdlib>
#include <string>

#include "harness.h"
#include "tm/api/relay_client.h"
#include "tm/net/tls.h"
#include "tm/net/url.h"
#include "tm/util/strings.h"

using tmirror::ErrorKind;
using tmirror::Result;
using tmirror::api::RelayClient;
using tmirror::api::RelayClientConfig;
using tmirror::net::ParseUrl;
using tmirror::net::TlsConfig;
using tmtest::FakeRelay;

namespace {

struct Certificate {
  std::string certificate_path;
  std::string key_path;
  std::string pem;
  bool valid = false;
};

std::string TempDirectory() {
  const char* base = std::getenv("TMPDIR");
  return base != nullptr ? base : "/tmp";
}

std::string ReadWholeFile(const std::string& path) {
  std::FILE* file = std::fopen(path.c_str(), "rb");
  if (file == nullptr) return std::string();
  std::string contents;
  char buffer[4096];
  while (true) {
    std::size_t read = std::fread(buffer, 1, sizeof(buffer), file);
    if (read == 0) break;
    contents.append(buffer, read);
  }
  std::fclose(file);
  return contents;
}

/// Generates a self-signed certificate for `common_name` with the openssl CLI.
/// Returns an invalid result when openssl is unavailable, so the test can skip.
Certificate MakeCertificate(const std::string& common_name, const std::string& suffix) {
  Certificate certificate;
  certificate.certificate_path = TempDirectory() + "/tm_tls_" + suffix + ".crt";
  certificate.key_path = TempDirectory() + "/tm_tls_" + suffix + ".key";

  std::string command =
      "openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj '/CN=" + common_name +
      "' -addext 'subjectAltName=DNS:" + common_name + ",IP:127.0.0.1' -keyout " +
      certificate.key_path + " -out " + certificate.certificate_path + " >/dev/null 2>&1";
  if (std::system(command.c_str()) != 0) return certificate;

  certificate.pem = ReadWholeFile(certificate.certificate_path);
  certificate.valid = !certificate.pem.empty();
  return certificate;
}

/// Performs one HTTPS request against the relay and reports the outcome.
tmirror::Status TryChallenge(const std::string& host, std::uint16_t port,
                             const TlsConfig& tls) {
  RelayClientConfig config;
  Result<tmirror::net::Url> url =
      ParseUrl("https://" + host + ":" + tmirror::Uint64ToString(port));
  if (!url.ok()) return url.status();
  config.base_url = url.value();
  config.tls = tls;
  config.connect_timeout_ms = 5000;
  config.request_timeout_ms = 5000;

  RelayClient client(config);
  tmirror::Bytes public_key(32, 0x01);
  Result<tmirror::api::Challenge> challenge = client.CreateChallenge(
      tmirror::crypto::ChallengeOperation::kRegisterIdentity,
      tmirror::crypto::kAlgorithmEd25519, tmirror::ByteView(public_key));
  return challenge.ok() ? tmirror::Status::Ok() : challenge.status();
}

}  // namespace

TM_TEST(Tls, TrustedCertificateConnectsAndUntrustedDoesNot) {
  Certificate certificate = MakeCertificate("localhost", "good");
  if (!certificate.valid) {
    std::fprintf(stderr, "  SKIP Tls: openssl is unavailable\n");
    return;
  }

  FakeRelay relay;
  if (!relay.Start({"--tls-cert", certificate.certificate_path,
                    "--tls-key", certificate.key_path})) {
    std::fprintf(stderr, "  SKIP Tls: the fake relay could not be started\n");
    return;
  }

  // 1. With the certificate as a trust anchor, the request succeeds.
  TlsConfig trusted;
  trusted.hostname = "localhost";
  trusted.trust_anchors_pem.push_back(certificate.pem);
  trusted.use_default_trust_store = false;
  tmirror::Status ok = TryChallenge("localhost", relay.port(), trusted);
  TM_CHECK_MSG(ok.ok(), "trusted connection failed: " + ok.ToString());

  // 2. Without it, verification fails — and fails as a TLS error, not as a generic
  //    network error, so the UI can say what actually went wrong (spec §15).
  TlsConfig untrusted;
  untrusted.hostname = "localhost";
  untrusted.use_default_trust_store = true;  // system anchors do not sign this cert
  tmirror::Status rejected = TryChallenge("localhost", relay.port(), untrusted);
  TM_CHECK(!rejected.ok());
  TM_CHECK_MSG(rejected.kind() == ErrorKind::kTlsFailure,
               "expected a TLS failure, got: " + rejected.ToString());
}

TM_TEST(Tls, HostnameMismatchIsRejected) {
  Certificate certificate = MakeCertificate("not-the-right-name.example", "wrongname");
  if (!certificate.valid) {
    std::fprintf(stderr, "  SKIP Tls: openssl is unavailable\n");
    return;
  }

  FakeRelay relay;
  if (!relay.Start({"--tls-cert", certificate.certificate_path,
                    "--tls-key", certificate.key_path})) {
    std::fprintf(stderr, "  SKIP Tls: the fake relay could not be started\n");
    return;
  }

  // The certificate is trusted, but it is for a different name. Anchoring a
  // certificate must not disable the hostname check.
  TlsConfig config;
  config.hostname = "localhost";
  config.trust_anchors_pem.push_back(certificate.pem);
  config.use_default_trust_store = false;
  tmirror::Status status = TryChallenge("localhost", relay.port(), config);
  TM_CHECK(!status.ok());
  TM_CHECK_MSG(status.kind() == ErrorKind::kTlsFailure,
               "expected a TLS failure, got: " + status.ToString());
}

TM_TEST(Tls, CleartextIsRefusedToANonLoopbackHost) {
  // spec §7.4: production connections are HTTPS/wss. A cleartext endpoint is only
  // reachable on loopback, where the bytes cannot leave the device.
  auto cancel = std::make_shared<tmirror::net::CancelSignal>();
  TlsConfig tls;
  tmirror::net::TransportOptions options;
  options.connect_timeout_ms = 1000;
  Result<std::unique_ptr<tmirror::net::Transport>> transport =
      tmirror::net::OpenTransport("http", "relay.example", 80, tls, cancel, options);
  TM_CHECK(!transport.ok());
  TM_CHECK(transport.status().kind() == ErrorKind::kTlsFailure);

  // And a TLS session cannot be established without any trust anchor at all.
  TM_CHECK(tmirror::net::IsLoopbackHost("127.0.0.1"));
  TM_CHECK(tmirror::net::IsLoopbackHost("localhost"));
  TM_CHECK(!tmirror::net::IsLoopbackHost("relay.example"));
}
