// The tunnel seam (spec §7.4).
//
// An embedded Tailscale node reaches its peers in user space, so it hands the client a
// connected descriptor rather than letting it call connect(). These tests drive the
// whole stack — HTTP, WebSocket, the API adapter, the controller — through that seam
// using an ordinary socket in place of the tunnel, so everything except Tailscale
// itself is covered without a tailnet.

#include <memory>
#include <mutex>
#include <string>

#include "harness.h"
#include "loopback_dialer.h"
#include "tm/api/relay_client.h"
#include "tm/app/controller.h"
#include "tm/app/persistence.h"
#include "tm/app/session.h"
#include "tm/net/http_client.h"
#include "tm/net/socket.h"
#include "tm/util/strings.h"

using tmirror::ErrorKind;
using tmirror::Result;
using tmirror::api::CredentialStore;
using tmirror::api::DeviceCredentials;
using tmirror::api::InMemorySecureStore;
using tmirror::app::AppConfig;
using tmirror::app::ConnectionState;
using tmirror::app::Controller;
using tmirror::app::ControllerCallbacks;
using tmirror::app::ExtractVisibleText;
using tmirror::app::Preferences;
using tmirror::app::SessionStatus;
using tmtest::FakeRelay;
using tmtest::LoopbackDialer;
using tmtest::WaitFor;

TM_TEST(Tunnel, AdoptedDescriptorBehavesLikeAConnectedSocket) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Tunnel: the fake relay could not be started\n");
    return;
  }

  LoopbackDialer dialer;
  Result<int> fd = dialer.DialFd("127.0.0.1", relay.port(), 5000);
  TM_REQUIRE(fd.ok());

  auto cancel = std::make_shared<tmirror::net::CancelSignal>();
  tmirror::net::TcpTransport transport(cancel);
  TM_REQUIRE(transport.Adopt(fd.value()).ok());
  TM_CHECK(transport.is_open());

  // A minimal request over the adopted descriptor: read and write must work exactly as
  // they do on a socket the transport connected itself.
  std::string request = "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
  TM_REQUIRE(transport.WriteAll(tmirror::ByteView(request), 5000).ok());

  std::string response;
  while (response.find("\r\n\r\n") == std::string::npos) {
    std::uint8_t buffer[512];
    Result<std::size_t> read = transport.Read(buffer, sizeof(buffer), 5000);
    TM_REQUIRE(read.ok());
    if (read.value() == 0) break;
    response.append(reinterpret_cast<const char*>(buffer), read.value());
  }
  TM_CHECK(response.find("200") != std::string::npos);
  TM_CHECK_EQ(dialer.dials.load(), 1);
}

TM_TEST(Tunnel, HttpRequestsGoThroughTheDialer) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Tunnel: the fake relay could not be started\n");
    return;
  }

  LoopbackDialer dialer;
  tmirror::net::HttpClientConfig config;
  config.scheme = "http";
  // A name that does not resolve: it can only work if the dialer is used, which is the
  // point — the tunnel resolves names, the client does not.
  config.host = "relay.internal.invalid";
  config.port = relay.port();
  config.dialer = &dialer;
  config.allow_cleartext_over_tunnel = true;
  config.request_timeout_ms = 5000;

  tmirror::net::HttpRequest request;
  request.method = "GET";
  request.target = "/healthz";

  tmirror::net::HttpClient client(config);
  Result<tmirror::net::HttpResponse> response = client.Send(request);
  TM_REQUIRE(response.ok());
  TM_CHECK_EQ(response.value().status, 200);
  TM_CHECK_EQ(dialer.dials.load(), 1);
  TM_CHECK_EQ(dialer.last_host(), "relay.internal.invalid");
  TM_CHECK_EQ(static_cast<int>(dialer.last_port()), static_cast<int>(relay.port()));
}

TM_TEST(Tunnel, CleartextThroughATunnelIsRefusedUnlessEnabled) {
  LoopbackDialer dialer;
  auto cancel = std::make_shared<tmirror::net::CancelSignal>();
  tmirror::net::TlsConfig tls;

  tmirror::net::TransportOptions options;
  options.dialer = &dialer;
  options.connect_timeout_ms = 1000;

  // The loopback exception must not be reachable through a tunnel: the descriptor is
  // not a loopback address, and cleartext would leave the device inside the tunnel.
  Result<std::unique_ptr<tmirror::net::Transport>> refused =
      tmirror::net::OpenTransport("http", "127.0.0.1", 9, tls, cancel, options);
  TM_CHECK(!refused.ok());
  TM_CHECK(refused.status().kind() == ErrorKind::kTlsFailure);
  TM_CHECK_EQ(dialer.dials.load(), 0);  // refused before dialling

  // A direct connection to loopback is still allowed, unchanged.
  tmirror::net::TransportOptions direct;
  direct.connect_timeout_ms = 500;
  Result<std::unique_ptr<tmirror::net::Transport>> allowed =
      tmirror::net::OpenTransport("http", "127.0.0.1", 9, tls, cancel, direct);
  // Port 9 refuses the connection, but the *policy* let it try, which is what matters.
  TM_CHECK(allowed.status().kind() != ErrorKind::kTlsFailure);
}

TM_TEST(Tunnel, ConnectionsAreRefusedWhileTheTunnelIsNotReady) {
  LoopbackDialer dialer;
  dialer.set_ready(false);

  auto cancel = std::make_shared<tmirror::net::CancelSignal>();
  tmirror::net::TlsConfig tls;
  tls.hostname = "relay.example";
  tmirror::net::TransportOptions options;
  options.dialer = &dialer;
  options.connect_timeout_ms = 1000;

  Result<std::unique_ptr<tmirror::net::Transport>> transport =
      tmirror::net::OpenTransport("https", "relay.example", 443, tls, cancel, options);
  TM_CHECK(!transport.ok());
  TM_CHECK(transport.status().kind() == ErrorKind::kNetworkUnavailable);
  // The message names the tunnel, so the user is told what is not ready.
  TM_CHECK(transport.status().message().find("test tunnel") != std::string::npos);
  TM_CHECK_EQ(dialer.dials.load(), 0);
}

TM_TEST(Tunnel, WholeSessionRunsThroughTheTunnel) {
  FakeRelay relay;
  if (!relay.Start()) {
    std::fprintf(stderr, "  SKIP Tunnel: the fake relay could not be started\n");
    return;
  }
  Result<FakeRelay::PairedDevice> paired = relay.PairClientDevice();
  TM_REQUIRE(paired.ok());
  std::string terminal_id = relay.CreateTerminal("tunnelled shell", 40, 10, true);
  TM_REQUIRE(!terminal_id.empty());

  LoopbackDialer dialer;
  AppConfig config;
  // The host name never resolves; every connection has to go through the dialer.
  config.server_url = "http://relay.internal.invalid:" + tmirror::Uint64ToString(relay.port());
  config.dialer = &dialer;
  config.allow_cleartext_over_tunnel = true;
  config.fallback_columns = 40;
  config.fallback_rows = 10;
  config.backoff.initial_delay_ms = 20;
  config.backoff.jitter = 0.0;
  config.snapshot_interval_ms = 1;

  InMemorySecureStore store;
  {
    DeviceCredentials credentials;
    credentials.server_url = config.server_url;
    credentials.identity_id = paired.value().identity_id;
    credentials.device_id = paired.value().device_id;
    credentials.private_key_seed = paired.value().device_seed;
    CredentialStore(&store).Save(credentials);
  }
  Preferences preferences("/tmp/tm_tunnel_prefs.json");

  std::mutex mutex;
  SessionStatus status;
  tmirror::term::SnapshotRef snapshot;
  ControllerCallbacks callbacks;
  callbacks.on_status = [&](const SessionStatus& value) {
    std::lock_guard<std::mutex> lock(mutex);
    status = value;
  };
  callbacks.on_frame = [&](const tmirror::term::SnapshotRef& value) {
    std::lock_guard<std::mutex> lock(mutex);
    snapshot = value;
  };

  Controller controller(config, &store, &preferences, callbacks);
  TM_REQUIRE(controller.Start().ok());
  controller.Attach(terminal_id);

  TM_REQUIRE(WaitFor([&] {
    std::lock_guard<std::mutex> lock(mutex);
    return status.state == ConnectionState::kAttached;
  }, 10000));

  relay.Emit(terminal_id, "through the tunnel\r\n");
  TM_CHECK(WaitFor([&] {
    std::lock_guard<std::mutex> lock(mutex);
    if (!snapshot) return false;
    return ExtractVisibleText(*snapshot).find("through the tunnel") != std::string::npos;
  }, 10000));

  // Authentication, discovery and the mirror upgrade were separate connections, and
  // every one of them went through the dialer.
  TM_CHECK_MSG(dialer.dials.load() >= 4,
               "dials: " + std::to_string(dialer.dials.load()));
  TM_CHECK_EQ(dialer.last_host(), "relay.internal.invalid");
  controller.Stop();
}
