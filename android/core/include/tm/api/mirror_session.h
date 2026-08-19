#pragma once

#include <functional>
#include <memory>
#include <string>

#include "tm/api/events.h"
#include "tm/net/url.h"
#include "tm/net/websocket.h"

namespace tmirror {
namespace api {

struct MirrorSessionConfig {
  net::Url base_url;
  net::TlsConfig tls;
  std::string terminal_id;
  /// Offer v2 (input-capable) and fall back to v1 if the deployment is older.
  bool offer_v1_fallback = true;
  /// Use a single-use ticket instead of the Authorization header. Native clients do
  /// not need this; it exists for proxies that strip the header (relay spec §5.1).
  bool use_websocket_ticket = false;
  Millis connect_timeout_ms = 15000;
  Millis idle_poll_ms = 250;
  std::size_t max_message_bytes = 8u * 1024u * 1024u;
  std::string user_agent = "TerminalMirror/0.1";
  /// Wakes a blocked read when the owner has outbound work, so input latency does
  /// not depend on the read timeout (spec §6.2).
  net::Notifier* interrupt = nullptr;
  /// Optional tunnel; see net::TransportOptions.
  net::Dialer* dialer = nullptr;
  bool allow_cleartext_over_tunnel = false;
};

/// One mirror WebSocket attachment (relay spec §6.2, §6.3).
///
/// Owns offset continuity, input sequencing and acknowledgement bookkeeping. All I/O
/// happens on whichever thread calls `Pump`/`SendInput`; the intended owner is the
/// network thread (spec §6.2).
class MirrorSession {
 public:
  using EventHandler = std::function<void(const MirrorEvent&)>;

  MirrorSession(MirrorSessionConfig config, EventHandler handler);
  ~MirrorSession();

  /// Connect and upgrade. `ticket_provider` is called only when the configuration
  /// asks for ticket authentication.
  Status Connect(const std::string& bearer_token, const std::string& ticket,
                 std::shared_ptr<net::CancelSignal> cancel);

  /// Send the single `subscribe` message. `from_offset < 0` requests the whole
  /// retained window (relay spec §6.2).
  Status Subscribe(bool have_offset, std::uint64_t from_offset);

  /// Read and dispatch at most one message. Returns a kTimeout status when nothing
  /// arrived within the deadline, which is not an error.
  Status Pump(Millis timeout_ms);

  /// Queue-free direct send of one input frame. Returns the client sequence used.
  /// Refuses when the subscription has no input authority, so a read-only mirror
  /// never emits a frame the relay would reject (relay spec §4.5).
  Result<std::uint64_t> SendInput(ByteView bytes);

  /// Ask the publisher to resize. It may decline; the authoritative size arrives as
  /// a `terminal.resize` (relay spec §6.3).
  Status RequestResize(std::uint32_t columns, std::uint32_t rows);

  Status SendPing();
  void Cancel();
  void Close(std::uint16_t code, const std::string& reason);

  bool subscribed() const { return subscribed_; }
  bool input_available() const { return input_available_; }
  bool protocol_v2() const { return protocol_v2_; }
  std::uint64_t next_expected_offset() const { return next_expected_offset_; }
  std::uint64_t durable_offset() const { return durable_offset_; }
  std::uint64_t accepted_through() const { return accepted_through_; }
  std::uint64_t unacknowledged_input_bytes() const { return unacknowledged_input_bytes_; }
  const RelayLimits& limits() const { return limits_; }
  const SubscribedInfo& info() const { return info_; }
  Millis last_message_monotonic_ms() const { return last_message_ms_; }
  const std::string& subprotocol() const;

  /// Path this session attaches to, also the path a ticket must be bound to.
  static std::string MirrorPath(const std::string& terminal_id);

  /// Decode one message. Public because these are pure protocol decoders with no
  /// socket involved, which is exactly how the ordering, duplication, gap and
  /// sequencing tests in spec §16.1 drive them.
  Status HandleControlMessage(const std::string& text);
  Status HandleBinaryFrame(const Bytes& payload);

 private:
  void Emit(const MirrorEvent& event);
  void ResetInputSequencing();

  MirrorSessionConfig config_;
  EventHandler handler_;
  std::unique_ptr<net::WebSocketClient> socket_;
  RelayLimits limits_;
  SubscribedInfo info_;
  bool subscribed_ = false;
  bool input_available_ = false;
  bool protocol_v2_ = false;
  std::uint64_t next_expected_offset_ = 0;
  std::uint64_t durable_offset_ = 0;
  std::uint64_t next_client_sequence_ = 1;
  std::uint64_t accepted_through_ = 0;
  std::uint64_t unacknowledged_input_bytes_ = 0;
  Millis last_message_ms_ = 0;
};

}  // namespace api
}  // namespace tmirror
