#pragma once

#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "tm/net/dialer.h"

namespace tmirror {
namespace net {

struct TailscaleConfig {
  /// App-private directory holding the node key. Must not be shared storage.
  std::string state_dir;
  /// Name the node takes in the tailnet.
  std::string hostname = "hypeterm";
  /// Optional pre-authorised key. Empty means the user authorises in a browser, and
  /// the login URL appears in the status.
  std::string auth_key;
  /// Optional self-hosted coordination server; empty uses the public one.
  std::string control_url;
};

struct TailscaleStatus {
  /// The embedded node was compiled into this build and its library loaded.
  bool available = false;
  bool started = false;
  /// Authenticated and able to carry traffic.
  bool running = false;
  std::string backend_state;
  /// Present while the node waits to be authorised; open it to finish login.
  std::string auth_url;
  std::string hostname;
  std::vector<std::string> addresses;
  int peers = 0;
  /// The node is not uploading diagnostics to Tailscale's log service. Reported by
  /// the node itself so the opt-out is verified rather than assumed.
  bool no_log_upload = false;
  /// Where the node may write. Android supplies neither by default; see the
  /// TailscaleDialer constructor.
  std::string cache_dir;
  std::string temp_dir;
  std::string last_error;
};

/// An embedded Tailscale node, used as a `Dialer`.
///
/// The node runs in user space (WireGuard over a netstack), so it needs no VpnService
/// and routes nothing but this app's own connections. `DialFd` returns one end of a
/// socketpair whose peer is pumped to and from a tailnet connection, which is why
/// every layer above keeps using an ordinary socket.
///
/// The Go implementation is a separate shared library, loaded on demand: it is roughly
/// 21 MB per ABI, and a build that omits it still runs — `available()` is then false
/// and connections through the tunnel are refused with an explanation rather than
/// falling back to the open internet.
///
/// Thread-safe. The controller starts and stops it from the UI thread while the
/// network thread dials through it.
class TailscaleDialer : public Dialer {
 public:
  explicit TailscaleDialer(TailscaleConfig config);
  ~TailscaleDialer() override;

  TailscaleDialer(const TailscaleDialer&) = delete;
  TailscaleDialer& operator=(const TailscaleDialer&) = delete;

  /// True when the embedded node is present in this build.
  bool available() const;

  /// Brings the node up with the settings given to the constructor. `auth_key` may be
  /// empty, in which case the node waits to be authorised in a browser and reports the
  /// URL in its status. Returns once the node has started, which is before it is
  /// authenticated: poll `GetStatus()` for `running` or an `auth_url`.
  Status Start(const std::string& auth_key);

  /// Stops the node and closes every tunnelled connection. Idempotent.
  void Stop();

  /// Stops the node and forgets its key, so the next start needs fresh authorisation.
  Status Logout();

  /// Named `GetStatus` rather than `Status` because `Status` is the result type.
  TailscaleStatus GetStatus() const;

  /// The network interfaces the node can see, as JSON.
  ///
  /// Diagnostic, and worth having: Android refuses Go's usual way of asking
  /// (`RTM_GETLINK`), so the node uses `getifaddrs` instead, and "can this device see
  /// its own interfaces" is the first question when a node will not come up.
  Result<std::string> InterfacesJson() const;

  // Dialer.
  Result<int> DialFd(const std::string& host, std::uint16_t port,
                     Millis timeout_ms) override;
  bool ready() const override;
  std::string name() const override;

  /// Overrides the library path. Only for tests; production loads it by SONAME from
  /// the application's native library directory.
  static void SetLibraryPathForTesting(const std::string& path);

 private:
  struct Library;

  std::shared_ptr<Library> Load() const;

  mutable std::mutex mutex_;
  /// Shared so a dial in flight keeps the entry points alive after the dialer is
  /// destroyed; the Go runtime itself is never unloaded.
  mutable std::shared_ptr<Library> library_;
  mutable bool load_attempted_ = false;
  bool started_ = false;
  TailscaleConfig config_;
  /// Last state reported, so a transition is logged once rather than every poll. The
  /// sentinel is not a state the node can report, so the first observation always logs.
  mutable std::string last_backend_state_ = "<unobserved>";
  mutable bool announced_auth_url_ = false;
};

}  // namespace net
}  // namespace tmirror
