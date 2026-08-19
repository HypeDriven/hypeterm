// The embedded Tailscale node (spec §7.4).
//
// These drive the real Go library through its real C API. Joining a tailnet needs
// credentials and a network, so what is covered here is everything up to that point:
// the library loads into this process, the node starts, its status decodes, dialling
// is refused until it is connected, and stopping it tears everything down. A build
// without the library must degrade to "unavailable", never to an unprotected direct
// connection.

#include <cstdlib>
#include <mutex>
#include <vector>
#include <string>

#include "harness.h"
#include "tm/net/tailscale_dialer.h"
#include "tm/util/json.h"
#include "tm/util/log.h"

using tmirror::ErrorKind;
using tmirror::Result;
using tmirror::net::TailscaleConfig;
using tmirror::net::TailscaleDialer;
using tmirror::net::TailscaleStatus;

namespace {

// Built by tools/build-tsnet-host.sh. Absent on a machine that has not run it, in
// which case the tests that need it skip rather than fail: the library is optional.
std::string HostLibraryPath() {
  const char* override_path = std::getenv("HYPETERM_TSNET_LIB");
  if (override_path != nullptr && *override_path != '\0') return override_path;
  const char* home = std::getenv("HOME");
  if (home == nullptr) return std::string();
  return std::string(home) + "/.cache/hypeterm/tsnet-host/libhypeterm_tsnet.so";
}

bool UseHostLibrary() {
  std::string path = HostLibraryPath();
  if (path.empty()) return false;
  TailscaleDialer::SetLibraryPathForTesting(path);
  TailscaleDialer probe{TailscaleConfig{}};
  if (probe.available()) return true;
  std::fprintf(stderr,
               "  SKIP Tailscale: no host library; run tools/build-tsnet-host.sh\n");
  return false;
}

std::string TestStateDir() { return "/tmp/hypeterm_tsnet_test"; }

}  // namespace

TM_TEST(Tailscale, AnAbsentLibraryIsReportedRatherThanBypassed) {
  TailscaleDialer::SetLibraryPathForTesting("/nonexistent/libhypeterm_tsnet.so");
  TailscaleConfig config;
  config.state_dir = TestStateDir();
  TailscaleDialer dialer(config);

  TM_CHECK(!dialer.available());
  TM_CHECK(!dialer.ready());
  // The name is what the user reads when a connection is refused, so it distinguishes
  // "not built in" from "not signed in yet".
  TM_CHECK_EQ(dialer.name(), "the Tailscale tunnel (not included in this build)");

  TailscaleStatus status = dialer.GetStatus();
  TM_CHECK(!status.available);
  TM_CHECK(!status.started);
  TM_CHECK(!status.running);
  TM_CHECK_EQ(status.backend_state, "Unavailable");

  tmirror::Status started = dialer.Start("");
  TM_CHECK(!started.ok());
  TM_CHECK(started.kind() == ErrorKind::kNetworkUnavailable);

  // The important part: no descriptor, so nothing above can accidentally connect
  // straight out to the internet in place of the tunnel.
  Result<int> fd = dialer.DialFd("relay.tailnet.ts.net", 443, 1000);
  TM_CHECK(!fd.ok());
  TM_CHECK(fd.status().kind() == ErrorKind::kNetworkUnavailable);

  // Logout on a build without the tunnel has nothing to do and must not fail.
  TM_CHECK(dialer.Logout().ok());
}

TM_TEST(Tailscale, TheNodeIsIdleUntilItIsStarted) {
  if (!UseHostLibrary()) return;
  TailscaleConfig config;
  config.state_dir = TestStateDir();
  TailscaleDialer dialer(config);

  TM_CHECK(dialer.available());
  TailscaleStatus status = dialer.GetStatus();
  TM_CHECK(status.available);
  TM_CHECK(!status.started);
  TM_CHECK(!status.running);
  TM_CHECK_EQ(status.backend_state, "Stopped");
  TM_CHECK_EQ(dialer.name(), "the Tailscale tunnel (not started)");
  TM_CHECK(status.auth_url.empty());
  TM_CHECK(status.addresses.empty());
  // tsnet ships node diagnostics to Tailscale's log service unless told not to; the
  // node reports its own opt-out so this stays a checked property (spec §9.3, §12).
  TM_CHECK(status.no_log_upload);

  // Not started, so no dial may be attempted.
  TM_CHECK(!dialer.ready());
  Result<int> fd = dialer.DialFd("relay.tailnet.ts.net", 443, 1000);
  TM_CHECK(!fd.ok());
  TM_CHECK(fd.status().kind() == ErrorKind::kNetworkUnavailable);
}

TM_TEST(Tailscale, StartingNeedsAPrivateStateDirectory) {
  if (!UseHostLibrary()) return;
  TailscaleDialer dialer{TailscaleConfig{}};  // state_dir left empty

  tmirror::Status started = dialer.Start("");
  TM_CHECK(!started.ok());
  TM_CHECK(started.kind() == ErrorKind::kInvalidArgument);
  TM_CHECK(!dialer.GetStatus().started);
}

TM_TEST(Tailscale, AStartedNodeCarriesNoTrafficUntilItIsAuthorised) {
  if (!UseHostLibrary()) return;
  TailscaleConfig config;
  config.state_dir = TestStateDir();
  config.hostname = "hypeterm-test";
  // An unreachable coordination server: the node starts, but can never reach Running,
  // which is exactly the state this test is about. No traffic leaves the machine.
  config.control_url = "http://127.0.0.1:1";
  TailscaleDialer dialer(config);

  tmirror::Status started = dialer.Start("");
  TM_CHECK_MSG(started.ok(), started.message());
  TM_REQUIRE(started.ok());

  TailscaleStatus status = dialer.GetStatus();
  TM_CHECK(status.available);
  TM_CHECK(status.started);
  TM_CHECK(!status.running);
  TM_CHECK(status.no_log_upload);

  // Starting twice is a no-op, not a second node.
  TM_CHECK(dialer.Start("").ok());

  // The node is up but not connected, so dialling is refused with a distinct message
  // from the "no library" case: the user is told to finish signing in, not that the
  // feature is missing.
  TM_CHECK(!dialer.ready());
  // Started but unreachable coordination server: neither "not built in" nor "not
  // started", and there is no login URL to offer either.
  TM_CHECK_EQ(dialer.name(), "the Tailscale tunnel (still connecting)");
  Result<int> fd = dialer.DialFd("relay.tailnet.ts.net", 443, 500);
  TM_CHECK(!fd.ok());
  TM_CHECK(fd.status().message().find("not connected") != std::string::npos);

  dialer.Stop();
  TM_CHECK(!dialer.GetStatus().started);
  // Stopping twice is safe.
  dialer.Stop();
}

TM_TEST(Tailscale, InterfacesAreEnumeratedWithoutNetlink) {
  if (!UseHostLibrary()) return;
  TailscaleConfig config;
  config.state_dir = TestStateDir();
  TailscaleDialer dialer(config);

  // Android refuses Go's RTM_GETLINK, so the node asks libc instead. This is the same
  // code path the device uses; if it were wrong, a node would never start.
  Result<std::string> document = dialer.InterfacesJson();
  TM_REQUIRE(document.ok());

  tmirror::Result<tmirror::Json> parsed = tmirror::Json::Parse(document.value());
  TM_REQUIRE(parsed.ok());
  TM_REQUIRE(parsed.value().is_array());
  TM_CHECK(!parsed.value().items().empty());

  bool saw_loopback = false;
  for (const tmirror::Json& item : parsed.value().items()) {
    TM_CHECK(item.is_object());
    // An index of zero means the name could not be resolved, which would make every
    // address ambiguous.
    std::uint64_t index = 0;
    TM_CHECK(item.GetUint64("index", &index));
    TM_CHECK(index >= 1);
    TM_CHECK(!item.GetString("name").empty());

    if (!item.GetBool("loopback", false)) continue;
    saw_loopback = true;
    const tmirror::Json* addresses = item.Find("addresses");
    TM_REQUIRE(addresses != nullptr && addresses->is_array());
    bool saw_localhost = false;
    for (const tmirror::Json& address : addresses->items()) {
      if (address.is_string() && address.string_value().rfind("127.0.0.1/", 0) == 0) {
        saw_localhost = true;
      }
    }
    TM_CHECK(saw_localhost);
  }
  TM_CHECK(saw_loopback);
}

TM_TEST(Tailscale, NothingTheNodeSaysAboutItselfReachesTheLog) {
  if (!UseHostLibrary()) return;

  // Everything the node reports about itself passes through the dialer, and one field
  // of it — the login URL — authorises a machine onto the tailnet. Whoever holds it
  // can join. It must never be written to a log (spec §9.3, §12, §15), and the way
  // that gets broken is someone logging the whole status document because it is
  // convenient. This watches the log for exactly that.
  std::vector<std::string> lines;
  std::mutex mutex;
  tmirror::Log::SetSink([&](tmirror::LogLevel, const std::string& tag,
                            const std::string& message) {
    std::lock_guard<std::mutex> lock(mutex);
    lines.push_back(tag + ": " + message);
  });
  tmirror::Log::SetLevel(tmirror::LogLevel::kVerbose);

  {
    TailscaleConfig config;
    config.state_dir = TestStateDir();
    config.hostname = "hypeterm-test";
    config.control_url = "http://127.0.0.1:1";
    TailscaleDialer dialer(config);

    dialer.Start("");
    // Several status reads, including the failure paths that used to dump the whole
    // document.
    for (int i = 0; i < 5; ++i) {
      dialer.GetStatus();
      dialer.name();
      dialer.ready();
    }
    dialer.DialFd("relay.tailnet.ts.net", 443, 200);
    dialer.Stop();
  }

  tmirror::Log::SetSink(nullptr);
  tmirror::Log::SetLevel(tmirror::LogLevel::kInfo);

  std::lock_guard<std::mutex> lock(mutex);
  for (const std::string& line : lines) {
    TM_CHECK_MSG(line.find("login.tailscale.com") == std::string::npos, line);
    TM_CHECK_MSG(line.find("auth_url") == std::string::npos, line);
    // The whole document leaking would show up as its JSON shape.
    TM_CHECK_MSG(line.find("\"no_log_upload\"") == std::string::npos, line);
  }
}
