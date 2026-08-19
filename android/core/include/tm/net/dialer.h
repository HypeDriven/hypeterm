#pragma once

#include <cstdint>
#include <string>

#include "tm/util/result.h"
#include "tm/util/time.h"

namespace tmirror {
namespace net {

/// Supplies an already-connected stream socket for a host and port.
///
/// A tunnel — an embedded Tailscale node, for instance — has no operating-system
/// socket to connect through: its transport lives in user space, and the peer is
/// reachable only inside that tunnel. Rather than teach every layer above about it,
/// the tunnel hands back a connected file descriptor (one end of a socketpair whose
/// other end it pumps) and the rest of the stack treats it as an ordinary socket.
///
/// Deliberately *not* a global: a dialer is passed down from the configuration, so a
/// connection that is meant to be direct can never silently take the tunnel, and a
/// test can substitute one without touching process state.
class Dialer {
 public:
  virtual ~Dialer() = default;

  /// A connected stream socket for `host`:`port`. Ownership of the descriptor passes
  /// to the caller, which closes it.
  virtual Result<int> DialFd(const std::string& host, std::uint16_t port,
                             Millis timeout_ms) = 0;

  /// False while the tunnel cannot carry traffic — not started, not authenticated,
  /// stopped. Callers surface this rather than attempting a connection.
  virtual bool ready() const = 0;

  /// Short description used in user-visible errors ("the Tailscale tunnel").
  virtual std::string name() const = 0;
};

}  // namespace net
}  // namespace tmirror
