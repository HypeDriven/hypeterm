#pragma once

#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include "tm/net/http_client.h"

namespace tmirror {
namespace net {

/// Application close codes the relay uses (relay spec §6, private range 4000-4999).
namespace ws_close {
constexpr std::uint16_t kNormal = 1000;
constexpr std::uint16_t kGoingAway = 1001;
constexpr std::uint16_t kProtocolError = 1002;
constexpr std::uint16_t kUnauthorized = 4001;
constexpr std::uint16_t kSuperseded = 4002;
constexpr std::uint16_t kSlowConsumer = 4003;
constexpr std::uint16_t kStorageUnavailable = 4004;
constexpr std::uint16_t kOffsetAhead = 4005;
constexpr std::uint16_t kRevoked = 4006;
constexpr std::uint16_t kServerShutdown = 4007;
constexpr std::uint16_t kLimitExceeded = 4008;
constexpr std::uint16_t kHeartbeatTimeout = 4009;
constexpr std::uint16_t kNotFound = 4011;
constexpr std::uint16_t kRateLimited = 4012;
constexpr std::uint16_t kFeatureDisabled = 4013;
constexpr std::uint16_t kHandshakeTimeout = 4014;
constexpr std::uint16_t kTerminalClosed = 4015;
}  // namespace ws_close

struct WebSocketOptions {
  /// Offered in preference order; the server echoes the one it selected.
  std::vector<std::string> subprotocols;
  /// Authorization: Bearer ... or x-relay-ticket: ... Never a query parameter
  /// (relay spec §4.3: token material must not appear in URLs).
  std::vector<HttpHeader> headers;
  std::size_t max_frame_bytes = 4u * 1024u * 1024u;
  std::size_t max_message_bytes = 8u * 1024u * 1024u;
  Millis handshake_timeout_ms = 15000;
};

struct WebSocketMessage {
  enum class Kind { kText, kBinary, kClose };
  Kind kind = Kind::kBinary;
  Bytes payload;
  std::uint16_t close_code = 0;
  std::string close_reason;

  std::string text() const { return StringFromBytes(payload); }
};

/// RFC 6455 client.
///
/// No extensions are negotiated: the relay states that no compression is available,
/// and without a negotiated compressor there is no decompression ratio to bound
/// (spec §7.4).
class WebSocketClient {
 public:
  WebSocketClient(HttpClientConfig config, WebSocketOptions options);
  ~WebSocketClient();

  Status Connect(const std::string& target, std::shared_ptr<CancelSignal> cancel);

  const std::string& subprotocol() const { return subprotocol_; }
  /// HTTP status of a failed upgrade, for mapping to a user-visible error (spec §15).
  int upgrade_status() const { return upgrade_status_; }

  /// Reads one complete message, answering pings transparently. A close frame is
  /// returned as a message so the caller can surface the code.
  Result<WebSocketMessage> ReadMessage(Millis timeout_ms);

  Status SendText(const std::string& text);
  Status SendBinary(ByteView payload);
  Status SendPing(ByteView payload = ByteView());
  Status SendPong(ByteView payload);
  Status SendClose(std::uint16_t code, const std::string& reason);

  void Cancel();
  void Close();
  /// Forwarded to the transport; safe before or after Connect.
  void SetInterrupt(Notifier* notifier);
  bool is_open() const { return transport_ && transport_->is_open() && !closed_; }

 private:
  struct Frame {
    bool fin = false;
    std::uint8_t opcode = 0;
    Bytes payload;
  };

  Result<Frame> ReadFrame(Millis timeout_ms);
  Status ReadExactly(std::size_t count, Millis timeout_ms);
  Status SendFrame(std::uint8_t opcode, ByteView payload);

  HttpClientConfig config_;
  WebSocketOptions options_;
  std::unique_ptr<Transport> transport_;
  std::shared_ptr<CancelSignal> cancel_;
  std::mutex write_mutex_;
  Bytes buffer_;
  std::size_t buffer_position_ = 0;
  Notifier* interrupt_ = nullptr;
  std::string subprotocol_;
  int upgrade_status_ = 0;
  bool closed_ = false;
};

}  // namespace net
}  // namespace tmirror
