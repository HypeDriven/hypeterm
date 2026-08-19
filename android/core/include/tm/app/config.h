#pragma once

#include <cstddef>
#include <string>

#include "tm/net/dialer.h"
#include "tm/net/tls.h"
#include "tm/term/emulator.h"
#include "tm/util/backoff.h"
#include "tm/util/result.h"

namespace tmirror {
namespace app {

/// Everything tunable about the client. Values that the relay reports (frame sizes,
/// heartbeat intervals, replay capacity) are deliberately absent: those are read from
/// the `ready` message rather than guessed (relay reconciliation §2.6).
struct AppConfig {
  /// Relay base URL, e.g. `https://relay.example`.
  std::string server_url;
  net::TlsConfig tls;
  std::string device_name = "Android device";
  std::string user_agent = "TerminalMirror/0.1";

  // ------------------------------------------------------------------ terminal
  int fallback_columns = 80;
  int fallback_rows = 24;
  term::Scrollback::Limits scrollback;
  term::Parser::Limits parser;
  /// Render whatever size the publisher is running at, and never ask it to change
  /// (spec §10.4).
  ///
  /// A phone asking a 200x50 desktop terminal to become 55x24 would reflow the
  /// session at the other end — where somebody is working — to suit the smaller
  /// screen. Following the remote size instead, and letting the user zoom and pan
  /// around it, leaves the far end untouched. The client still computes a local grid
  /// as a fallback for a relay that reports no size at all.
  bool follow_remote_size = true;
  /// OSC 52 clipboard writes stay off until a deployment reviews them (spec §8.1).
  bool allow_clipboard_write = false;
  bool answer_device_queries = true;

  // ------------------------------------------------------------------- queues
  /// Bounded, and never silently dropped: a full queue is reported (spec §6.2).
  std::size_t command_queue_depth = 512;
  std::size_t pending_input_bytes = 256 * 1024;
  /// Chunk size for pasted text.
  std::size_t paste_chunk_bytes = 2048;
  std::size_t paste_max_bytes = 1024 * 1024;

  // ------------------------------------------------------------------ timing
  Millis resize_debounce_ms = 250;
  Millis snapshot_interval_ms = 16;
  Millis connect_timeout_ms = 15000;
  Millis request_timeout_ms = 30000;
  /// Fallback heartbeat when the relay does not advertise one.
  Millis heartbeat_timeout_ms = 60000;
  Backoff::Options backoff;

  // --------------------------------------------------------------- lifecycle
  /// spec §11: keep the connection while backgrounding is useful, then detach.
  bool detach_when_backgrounded = true;
  Millis background_grace_ms = 30000;

  // ------------------------------------------------------------------ tunnel
  /// Dial every relay connection through this tunnel instead of the network stack.
  /// Not owned: the host layer creates it (an embedded Tailscale node, for example)
  /// and keeps it alive for the controller's lifetime.
  net::Dialer* dialer = nullptr;
  /// Allow http:// and ws:// through the tunnel. Off by default; see
  /// net::TransportOptions for why this is a deliberate choice rather than a default.
  bool allow_cleartext_over_tunnel = false;

  // ---------------------------------------------------------------- security
  /// Applies Android's secure-window flag to the terminal screen (spec §12).
  bool secure_window = false;

  Status Validate() const;
};

}  // namespace app
}  // namespace tmirror
