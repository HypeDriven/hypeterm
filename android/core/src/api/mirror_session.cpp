#include "tm/api/mirror_session.h"

#include <cstring>

#include "tm/util/json.h"
#include "tm/util/log.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace api {
namespace {

constexpr const char kTag[] = "tm.mirror";
constexpr std::uint8_t kFrameOutput = 0x01;
constexpr std::uint8_t kFrameInput = 0x02;

std::uint64_t ReadUint64BigEndian(const std::uint8_t* data) {
  std::uint64_t value = 0;
  for (int i = 0; i < 8; ++i) {
    value = (value << 8) | data[i];
  }
  return value;
}

void WriteUint64BigEndian(std::uint64_t value, std::uint8_t* out) {
  for (int i = 0; i < 8; ++i) {
    out[i] = static_cast<std::uint8_t>((value >> (56 - 8 * i)) & 0xFF);
  }
}

std::uint32_t ReadDimension(const Json& object, const std::string& key) {
  // Server-provided sizes are untrusted (spec §12): clamp before they reach the grid.
  std::uint32_t value = 0;
  const Json* field = object.Find(key);
  if (field != nullptr && field->AsUint32Bounded(10000, &value)) return value;
  return 0;
}

}  // namespace

MirrorSession::MirrorSession(MirrorSessionConfig config, EventHandler handler)
    : config_(std::move(config)), handler_(std::move(handler)) {}

MirrorSession::~MirrorSession() = default;

std::string MirrorSession::MirrorPath(const std::string& terminal_id) {
  return "/v1/terminals/" + UrlEncode(terminal_id) + "/mirror";
}

const std::string& MirrorSession::subprotocol() const {
  static const std::string kEmpty;
  return socket_ ? socket_->subprotocol() : kEmpty;
}

Status MirrorSession::Connect(const std::string& bearer_token, const std::string& ticket,
                              std::shared_ptr<net::CancelSignal> cancel) {
  net::HttpClientConfig http;
  http.scheme = config_.base_url.scheme == "https" || config_.base_url.scheme == "wss" ? "wss"
                                                                                      : "ws";
  http.host = config_.base_url.host;
  http.port = config_.base_url.port;
  http.tls = config_.tls;
  if (http.tls.hostname.empty()) http.tls.hostname = http.host;
  http.connect_timeout_ms = config_.connect_timeout_ms;
  http.user_agent = config_.user_agent;
  http.dialer = config_.dialer;
  http.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;

  net::WebSocketOptions options;
  options.subprotocols.push_back(kMirrorSubprotocolV2);
  if (config_.offer_v1_fallback) options.subprotocols.push_back(kMirrorSubprotocolV1);
  options.max_message_bytes = config_.max_message_bytes;
  options.max_frame_bytes = config_.max_message_bytes;
  options.handshake_timeout_ms = config_.connect_timeout_ms;

  if (config_.use_websocket_ticket) {
    if (ticket.empty()) {
      return Status::Error(ErrorKind::kAuthFailed, "a websocket ticket was required");
    }
    options.headers.push_back({"x-relay-ticket", ticket});
  } else {
    if (bearer_token.empty()) {
      return Status::Error(ErrorKind::kAuthFailed, "no access token for the mirror upgrade");
    }
    options.headers.push_back({"Authorization", "Bearer " + bearer_token});
  }

  socket_ = std::make_unique<net::WebSocketClient>(http, options);
  Status status = socket_->Connect(MirrorPath(config_.terminal_id), std::move(cancel));
  if (!status.ok()) {
    socket_.reset();
    return status;
  }

  socket_->SetInterrupt(config_.interrupt);
  protocol_v2_ = socket_->subprotocol() == kMirrorSubprotocolV2;
  limits_.input_supported = protocol_v2_;
  subscribed_ = false;
  input_available_ = false;
  next_expected_offset_ = 0;
  durable_offset_ = 0;
  ResetInputSequencing();
  last_message_ms_ = Clock::System()->MonotonicMillis();
  TM_LOG_INFO(kTag, "mirror attached with subprotocol %s", socket_->subprotocol().c_str());
  return Status::Ok();
}

void MirrorSession::ResetInputSequencing() {
  // The relay's expected sequence is exactly `accepted_through + 1`: it advances only
  // when a frame is accepted (relay spec §6.3). Deriving it means a refusal never
  // needs the human-readable message parsed.
  next_client_sequence_ = accepted_through_ + 1;
  unacknowledged_input_bytes_ = 0;
}

Status MirrorSession::Subscribe(bool have_offset, std::uint64_t from_offset) {
  if (!socket_) return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not connected");
  Json message = Json::Object();
  message.Set("type", Json::String("subscribe"));
  if (have_offset) message.Set("from_offset", Json::Uint(from_offset));
  return socket_->SendText(message.Serialize());
}

void MirrorSession::Emit(const MirrorEvent& event) {
  if (handler_) handler_(event);
}

Status MirrorSession::Pump(Millis timeout_ms) {
  if (!socket_) return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not connected");
  Result<net::WebSocketMessage> message = socket_->ReadMessage(timeout_ms);
  if (!message.ok()) return message.status();

  last_message_ms_ = Clock::System()->MonotonicMillis();
  switch (message.value().kind) {
    case net::WebSocketMessage::Kind::kText:
      return HandleControlMessage(message.value().text());
    case net::WebSocketMessage::Kind::kBinary:
      return HandleBinaryFrame(message.value().payload);
    case net::WebSocketMessage::Kind::kClose: {
      MirrorEvent event;
      event.kind = MirrorEventKind::kError;
      std::uint16_t code = message.value().close_code;
      ErrorKind kind = ErrorKind::kNetworkUnavailable;
      std::string reason = message.value().close_reason;
      switch (code) {
        case net::ws_close::kUnauthorized: kind = ErrorKind::kAuthFailed; break;
        case net::ws_close::kRevoked: kind = ErrorKind::kAuthFailed; break;
        case net::ws_close::kNotFound: kind = ErrorKind::kNotFound; break;
        case net::ws_close::kOffsetAhead: kind = ErrorKind::kSyncFailure; break;
        case net::ws_close::kSlowConsumer: kind = ErrorKind::kSyncFailure; break;
        case net::ws_close::kTerminalClosed: kind = ErrorKind::kTerminalClosed; break;
        case net::ws_close::kRateLimited: kind = ErrorKind::kRateLimited; break;
        case net::ws_close::kFeatureDisabled: kind = ErrorKind::kPermissionDenied; break;
        case net::ws_close::kLimitExceeded: kind = ErrorKind::kProtocolError; break;
        case net::ws_close::kProtocolError: kind = ErrorKind::kProtocolError; break;
        case net::ws_close::kNormal: kind = ErrorKind::kNone; break;
        default: break;
      }
      event.code = "closed_" + Int64ToString(code);
      event.message = reason;
      event.status = Status::Error(kind == ErrorKind::kNone ? ErrorKind::kNetworkUnavailable
                                                            : kind,
                                   "mirror connection closed (" + Int64ToString(code) + ")")
                         .set_code(event.code);
      Emit(event);
      return event.status;
    }
  }
  return Status::Ok();
}

Status MirrorSession::HandleControlMessage(const std::string& text) {
  Json::Limits limits;
  limits.max_bytes = static_cast<std::size_t>(limits_.max_control_message_bytes);
  if (limits.max_bytes < 4096) limits.max_bytes = 65536;
  Result<Json> parsed = Json::Parse(text, limits);
  if (!parsed.ok()) return parsed.status();
  const Json& message = parsed.value();
  if (!message.is_object()) {
    return Status::Error(ErrorKind::kProtocolError, "control message is not an object");
  }
  const std::string type = message.GetString("type");

  MirrorEvent event;
  if (type == "ready") {
    event.kind = MirrorEventKind::kReady;
    const Json* limits_json = message.Find("limits");
    if (limits_json != nullptr && limits_json->is_object()) {
      limits_json->GetUint64("max_output_frame_bytes", &limits_.max_output_frame_bytes);
      limits_json->GetUint64("max_control_message_bytes", &limits_.max_control_message_bytes);
      limits_json->GetUint64("replay_capacity_bytes", &limits_.replay_capacity_bytes);
      limits_json->GetUint64("heartbeat_interval_seconds", &limits_.heartbeat_interval_seconds);
      limits_json->GetUint64("heartbeat_timeout_seconds", &limits_.heartbeat_timeout_seconds);
      std::uint64_t input_frame = 0;
      if (limits_json->GetUint64("max_input_frame_bytes", &input_frame) && input_frame > 0) {
        limits_.max_input_frame_bytes = input_frame;
      }
    }
    // The relay states its own limits; hard-coding them would drift (relay
    // reconciliation §2.6). Only sanity bounds are applied here.
    if (limits_.max_input_frame_bytes < 16) limits_.max_input_frame_bytes = 16;
    if (limits_.replay_capacity_bytes > kReplayWindowBytes) {
      limits_.replay_capacity_bytes = kReplayWindowBytes;
    }
    event.limits = limits_;
    Emit(event);
    return Status::Ok();
  }

  if (type == "subscribed") {
    info_ = SubscribedInfo();
    info_.terminal_id = message.GetString("terminal_id");
    message.GetUint64("requested_from_offset", &info_.requested_from_offset);
    message.GetUint64("replay_start_offset", &info_.replay_start_offset);
    message.GetUint64("next_offset", &info_.next_offset);
    message.GetUint64("durable_offset", &info_.durable_offset);
    message.GetUint64("earliest_offset", &info_.earliest_offset);
    info_.terminal_state = message.GetString("terminal_state");
    info_.label = SanitizeForMessage(message.GetString("label"), 128);
    info_.term = SanitizeForMessage(message.GetString("term"), 64);
    info_.columns = ReadDimension(message, "cols");
    info_.rows = ReadDimension(message, "rows");
    // Both fields are omitted for a version 1 subscriber, in which case input is
    // simply not available (relay spec §6.2).
    message.GetOptionalBool("accepts_input", &info_.accepts_input);
    message.GetOptionalBool("input_available", &info_.input_available);

    if (info_.next_offset < info_.replay_start_offset ||
        info_.durable_offset > info_.next_offset) {
      return Status::Error(ErrorKind::kProtocolError,
                           "subscribed offsets are inconsistent")
          .set_code("invalid_message");
    }

    subscribed_ = true;
    input_available_ = protocol_v2_ && info_.input_available;
    next_expected_offset_ = info_.replay_start_offset;
    durable_offset_ = info_.durable_offset;
    accepted_through_ = 0;
    ResetInputSequencing();

    event.kind = MirrorEventKind::kSubscribed;
    event.subscribed = info_;
    Emit(event);
    return Status::Ok();
  }

  if (type == "gap") {
    event.kind = MirrorEventKind::kGap;
    message.GetUint64("requested_from_offset", &event.requested_from_offset);
    message.GetUint64("available_from_offset", &event.available_from_offset);
    // Replay resumes at the available offset; everything before it is unrecoverable,
    // so the emulator must be reset by the handler (relay spec §6.2).
    next_expected_offset_ = event.available_from_offset;
    Emit(event);
    return Status::Ok();
  }

  if (type == "durable") {
    std::uint64_t durable = 0;
    message.GetUint64("durable_offset", &durable);
    if (durable > durable_offset_) durable_offset_ = durable;
    event.kind = MirrorEventKind::kDurable;
    event.durable_offset = durable_offset_;
    Emit(event);
    return Status::Ok();
  }

  if (type == "terminal.resize") {
    event.kind = MirrorEventKind::kResize;
    event.columns = ReadDimension(message, "cols");
    event.rows = ReadDimension(message, "rows");
    if (event.columns == 0 || event.rows == 0) return Status::Ok();  // ignore nonsense
    info_.columns = event.columns;
    info_.rows = event.rows;
    Emit(event);
    return Status::Ok();
  }

  if (type == "terminal.closed") {
    event.kind = MirrorEventKind::kTerminalClosed;
    event.code = SanitizeForMessage(message.GetString("reason"), 64);
    message.GetUint64("next_offset", &event.start_offset);
    message.GetUint64("durable_offset", &event.durable_offset);
    Emit(event);
    return Status::Ok();
  }

  if (type == "input.ack") {
    std::uint64_t accepted = 0;
    message.GetUint64("accepted_through", &accepted);
    message.GetUint64("relay_sequence", &event.relay_sequence);
    if (accepted > accepted_through_) accepted_through_ = accepted;
    if (accepted_through_ + 1 > next_client_sequence_) {
      next_client_sequence_ = accepted_through_ + 1;
    }
    unacknowledged_input_bytes_ = 0;
    event.kind = MirrorEventKind::kInputAck;
    event.accepted_through = accepted_through_;
    Emit(event);
    return Status::Ok();
  }

  if (type == "error") {
    event.kind = MirrorEventKind::kError;
    event.code = message.GetString("code");
    event.message = SanitizeForMessage(message.GetString("message"), 200);
    event.status = Status::Error(ErrorKindForRelayCode(event.code), event.message)
                       .set_code(event.code);
    if (StartsWith(event.code, "input_") || event.code == "rate_limited") {
      // Nothing after a refused frame was accepted either, so resynchronise the
      // sequence and let the controller tell the user (spec §9.3: input is never
      // silently replayed).
      ResetInputSequencing();
    }
    Emit(event);
    // Input refusals leave the subscription usable; everything else is fatal for it.
    if (StartsWith(event.code, "input_") || event.code == "rate_limited") {
      return Status::Ok();
    }
    return event.status;
  }

  if (type == "notice") {
    event.kind = MirrorEventKind::kNotice;
    event.code = message.GetString("code");
    event.message = SanitizeForMessage(message.GetString("message"), 200);
    Emit(event);
    return Status::Ok();
  }

  if (type == "ping") {
    Json pong = Json::Object();
    pong.Set("type", Json::String("pong"));
    return socket_->SendText(pong.Serialize());
  }

  if (type == "pong") return Status::Ok();

  // An unknown type is fatal unless the sender marked it ignorable — the rule is
  // symmetric with the server's (relay reconciliation §2.12).
  if (message.GetBool("optional", false)) {
    TM_LOG_DEBUG(kTag, "ignoring optional control message");
    return Status::Ok();
  }
  return Status::Error(ErrorKind::kProtocolIncompatible,
                       "unknown control message type: " + SanitizeForMessage(type, 40))
      .set_code("unknown_message_type");
}

Status MirrorSession::HandleBinaryFrame(const Bytes& payload) {
  if (payload.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "empty binary frame");
  }
  if (payload[0] != kFrameOutput) {
    return Status::Error(ErrorKind::kProtocolError, "unknown binary frame type")
        .set_code("invalid_message");
  }
  if (payload.size() < 9) {
    return Status::Error(ErrorKind::kProtocolError, "truncated output frame");
  }
  // Zero-length payloads are never sent (relay spec §6.2); one would desynchronise
  // nothing but is still malformed.
  if (payload.size() == 9) {
    return Status::Error(ErrorKind::kProtocolError, "zero-length output frame");
  }

  std::uint64_t start = ReadUint64BigEndian(payload.data() + 1);
  const std::uint8_t* data = payload.data() + 9;
  std::size_t length = payload.size() - 9;

  if (start + length < start) {
    return Status::Error(ErrorKind::kProtocolError, "output frame offset overflows");
  }

  if (start > next_expected_offset_) {
    // The relay guarantees no gap between replay and live delivery, so a jump means
    // the stream is not trustworthy: report rather than render corrupt state
    // (spec §7.3).
    return Status::Error(ErrorKind::kSyncFailure,
                         "output stream skipped from " + Uint64ToString(next_expected_offset_) +
                             " to " + Uint64ToString(start))
        .set_code("offset_gap");
  }

  if (start + length <= next_expected_offset_) {
    // Wholly duplicated bytes: apply nothing (spec §7.3).
    return Status::Ok();
  }

  std::size_t skip = static_cast<std::size_t>(next_expected_offset_ - start);
  MirrorEvent event;
  event.kind = MirrorEventKind::kOutput;
  event.start_offset = next_expected_offset_;
  event.payload = ByteView(data + skip, length - skip);
  next_expected_offset_ = start + length;
  Emit(event);
  return Status::Ok();
}

Result<std::uint64_t> MirrorSession::SendInput(ByteView bytes) {
  if (!subscribed_) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not attached");
  }
  if (bytes.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument, "input frames must not be empty");
  }
  // Checked before the socket: being attached read-only is a property of the
  // subscription, and the caller needs that distinction to tell the user why.
  if (!input_available_) {
    // A read-only attachment: refuse locally rather than let the relay refuse and
    // count it against the rate limit (relay spec §4.5).
    return Status::Error(ErrorKind::kInputRefused,
                         "this session is attached read-only")
        .set_code("input_not_available");
  }
  if (bytes.size() > limits_.max_input_frame_bytes) {
    return Status::Error(ErrorKind::kInvalidArgument, "input frame exceeds the negotiated limit")
        .set_code("limit_exceeded");
  }

  if (!socket_) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not connected");
  }

  Bytes frame(9 + bytes.size());
  frame[0] = kFrameInput;
  WriteUint64BigEndian(next_client_sequence_, frame.data() + 1);
  std::memcpy(frame.data() + 9, bytes.data(), bytes.size());

  Status status = socket_->SendBinary(ByteView(frame));
  // Keystrokes must never reach a log (spec §9.3): only the byte count is recorded.
  if (!status.ok()) {
    SecureZero(frame);
    return status;
  }
  std::uint64_t sequence = next_client_sequence_++;
  unacknowledged_input_bytes_ += bytes.size();
  SecureZero(frame);
  return sequence;
}

Status MirrorSession::RequestResize(std::uint32_t columns, std::uint32_t rows) {
  if (!socket_ || !subscribed_) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not attached");
  }
  if (!input_available_) {
    return Status::Error(ErrorKind::kInputRefused,
                         "resize requests need input authority")
        .set_code("input_not_available");
  }
  Json message = Json::Object();
  message.Set("type", Json::String("terminal.resize_request"));
  message.Set("cols", Json::Uint(columns));
  message.Set("rows", Json::Uint(rows));
  return socket_->SendText(message.Serialize());
}

Status MirrorSession::SendPing() {
  if (!socket_) return Status::Error(ErrorKind::kNetworkUnavailable, "mirror is not connected");
  return socket_->SendPing();
}

void MirrorSession::Cancel() {
  if (socket_) socket_->Cancel();
}

void MirrorSession::Close(std::uint16_t code, const std::string& reason) {
  if (!socket_) return;
  socket_->SendClose(code, reason);
  socket_->Close();
  subscribed_ = false;
  input_available_ = false;
}

}  // namespace api
}  // namespace tmirror
