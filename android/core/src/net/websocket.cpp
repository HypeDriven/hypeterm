#include "tm/net/websocket.h"

#include <cstring>

#include "tm/crypto/crypto.h"
#include "tm/util/base64.h"
#include "tm/util/log.h"
#include "tm/util/random.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace net {
namespace {

constexpr const char kTag[] = "tm.ws";
constexpr const char kHandshakeGuid[] = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

constexpr std::uint8_t kOpcodeContinuation = 0x0;
constexpr std::uint8_t kOpcodeText = 0x1;
constexpr std::uint8_t kOpcodeBinary = 0x2;
constexpr std::uint8_t kOpcodeClose = 0x8;
constexpr std::uint8_t kOpcodePing = 0x9;
constexpr std::uint8_t kOpcodePong = 0xA;

Status ProtocolError(const std::string& message) {
  return Status::Error(ErrorKind::kProtocolError, "websocket: " + message);
}

}  // namespace

WebSocketClient::WebSocketClient(HttpClientConfig config, WebSocketOptions options)
    : config_(std::move(config)), options_(std::move(options)) {}

WebSocketClient::~WebSocketClient() { Close(); }

Status WebSocketClient::Connect(const std::string& target,
                                std::shared_ptr<CancelSignal> cancel) {
  cancel_ = cancel ? std::move(cancel) : std::make_shared<CancelSignal>();

  Result<std::unique_ptr<HttpConnection>> connection = HttpConnection::Open(config_, cancel_);
  if (!connection.ok()) return connection.status();
  HttpConnection* http = connection.value().get();

  Bytes key_bytes = SecureRandomBytes(16);
  if (key_bytes.size() != 16) {
    return Status::Error(ErrorKind::kInternal, "websocket: no secure random source");
  }
  std::string key = Base64Encode(ByteView(key_bytes));

  HttpRequest request;
  request.method = "GET";
  request.target = target;
  request.headers.push_back({"Upgrade", "websocket"});
  request.headers.push_back({"Connection", "Upgrade"});
  request.headers.push_back({"Sec-WebSocket-Version", "13"});
  request.headers.push_back({"Sec-WebSocket-Key", key});
  if (!options_.subprotocols.empty()) {
    request.headers.push_back({"Sec-WebSocket-Protocol", Join(options_.subprotocols, ", ")});
  }
  for (const HttpHeader& header : options_.headers) request.headers.push_back(header);

  Status status = http->WriteRequest(request, config_, options_.handshake_timeout_ms);
  if (!status.ok()) return status;

  Result<HttpResponse> response =
      http->ReadResponseHead(options_.handshake_timeout_ms, config_.max_header_bytes);
  if (!response.ok()) return response.status();
  upgrade_status_ = response.value().status;

  if (response.value().status != 101) {
    // Read the error body so the caller can map the relay's error code (spec §15).
    Status body = http->ReadBody(&response.value(), options_.handshake_timeout_ms,
                                 config_.max_response_bytes);
    (void)body;
    ErrorKind kind = ErrorKind::kProtocolError;
    switch (response.value().status) {
      case 401:
      case 403: kind = ErrorKind::kAuthFailed; break;
      case 404: kind = ErrorKind::kNotFound; break;
      case 426: kind = ErrorKind::kProtocolIncompatible; break;
      case 429: kind = ErrorKind::kRateLimited; break;
      default:
        if (response.value().status >= 500) kind = ErrorKind::kServerError;
        break;
    }
    // The body is the relay's error envelope. It goes in the message, sanitised; the
    // code field stays for machine-readable codes the caller sets from the envelope.
    std::string detail = SanitizeForMessage(response.value().body, 200);
    return Status::Error(kind, "websocket upgrade rejected with status " +
                                   Int64ToString(response.value().status) +
                                   (detail.empty() ? "" : ": " + detail));
  }

  if (!EqualsIgnoreCaseAscii(Trim(response.value().Header("upgrade")), "websocket")) {
    return ProtocolError("server did not upgrade the connection");
  }
  std::string connection_header = ToLowerAscii(response.value().Header("connection"));
  if (connection_header.find("upgrade") == std::string::npos) {
    return ProtocolError("server did not confirm the upgrade");
  }

  std::string expected =
      Base64Encode(ByteView(crypto::Sha1(ByteView(key + kHandshakeGuid))));
  if (response.value().Header("sec-websocket-accept") != expected) {
    return ProtocolError("handshake accept value does not match");
  }

  subprotocol_ = Trim(response.value().Header("sec-websocket-protocol"));
  if (!options_.subprotocols.empty()) {
    bool offered = false;
    for (const std::string& candidate : options_.subprotocols) {
      if (candidate == subprotocol_) offered = true;
    }
    if (!offered) {
      return Status::Error(ErrorKind::kProtocolIncompatible,
                           "websocket: server selected a subprotocol that was not offered");
    }
  }
  // Any extension would need a decompression bound we cannot verify (spec §7.4).
  if (!Trim(response.value().Header("sec-websocket-extensions")).empty()) {
    return Status::Error(ErrorKind::kProtocolIncompatible,
                         "websocket: server negotiated an extension that was not offered");
  }

  buffer_ = http->TakeBuffered();
  buffer_position_ = 0;
  transport_ = http->TakeTransport();
  if (interrupt_ != nullptr) transport_->SetInterrupt(interrupt_);
  closed_ = false;
  TM_LOG_DEBUG(kTag, "connected, subprotocol=%s", subprotocol_.c_str());
  return Status::Ok();
}

Status WebSocketClient::ReadExactly(std::size_t count, Millis timeout_ms) {
  while (buffer_.size() - buffer_position_ < count) {
    if (buffer_position_ > 0) {
      buffer_.erase(buffer_.begin(), buffer_.begin() + static_cast<std::ptrdiff_t>(buffer_position_));
      buffer_position_ = 0;
    }
    std::uint8_t chunk[16384];
    Result<std::size_t> read = transport_->Read(chunk, sizeof(chunk), timeout_ms);
    if (!read.ok()) return read.status();
    if (read.value() == 0) {
      return Status::Error(ErrorKind::kNetworkUnavailable,
                           "websocket: connection closed by the server");
    }
    buffer_.insert(buffer_.end(), chunk, chunk + read.value());
  }
  return Status::Ok();
}

Result<WebSocketClient::Frame> WebSocketClient::ReadFrame(Millis timeout_ms) {
  Status status = ReadExactly(2, timeout_ms);
  if (!status.ok()) return status;

  std::uint8_t byte0 = buffer_[buffer_position_];
  std::uint8_t byte1 = buffer_[buffer_position_ + 1];
  buffer_position_ += 2;

  Frame frame;
  frame.fin = (byte0 & 0x80) != 0;
  if ((byte0 & 0x70) != 0) return ProtocolError("reserved frame bits are set");
  frame.opcode = static_cast<std::uint8_t>(byte0 & 0x0F);
  bool masked = (byte1 & 0x80) != 0;
  if (masked) return ProtocolError("server frames must not be masked");

  std::uint64_t length = byte1 & 0x7F;
  if (length == 126) {
    status = ReadExactly(2, timeout_ms);
    if (!status.ok()) return status;
    length = (static_cast<std::uint64_t>(buffer_[buffer_position_]) << 8) |
             buffer_[buffer_position_ + 1];
    buffer_position_ += 2;
  } else if (length == 127) {
    status = ReadExactly(8, timeout_ms);
    if (!status.ok()) return status;
    length = 0;
    for (int i = 0; i < 8; ++i) {
      length = (length << 8) | buffer_[buffer_position_ + static_cast<std::size_t>(i)];
    }
    buffer_position_ += 8;
  }

  if (length > options_.max_frame_bytes) {
    return Status::Error(ErrorKind::kProtocolError, "websocket: frame exceeds the size limit");
  }
  bool is_control = (frame.opcode & 0x08) != 0;
  if (is_control) {
    if (length > 125) return ProtocolError("control frame payload is too long");
    if (!frame.fin) return ProtocolError("control frames must not be fragmented");
  }

  if (length > 0) {
    status = ReadExactly(static_cast<std::size_t>(length), timeout_ms);
    if (!status.ok()) return status;
    frame.payload.assign(buffer_.begin() + static_cast<std::ptrdiff_t>(buffer_position_),
                         buffer_.begin() + static_cast<std::ptrdiff_t>(buffer_position_ + length));
    buffer_position_ += static_cast<std::size_t>(length);
  }
  return frame;
}

Result<WebSocketMessage> WebSocketClient::ReadMessage(Millis timeout_ms) {
  Bytes payload;
  std::uint8_t message_opcode = 0;
  bool in_message = false;

  while (true) {
    Result<Frame> frame = ReadFrame(timeout_ms);
    if (!frame.ok()) return frame.status();
    Frame& value = frame.value();

    switch (value.opcode) {
      case kOpcodePing: {
        Status sent = SendPong(ByteView(value.payload));
        if (!sent.ok()) return sent;
        continue;
      }
      case kOpcodePong:
        continue;
      case kOpcodeClose: {
        WebSocketMessage message;
        message.kind = WebSocketMessage::Kind::kClose;
        if (value.payload.size() >= 2) {
          message.close_code = static_cast<std::uint16_t>(
              (static_cast<std::uint16_t>(value.payload[0]) << 8) | value.payload[1]);
          message.close_reason = SanitizeForMessage(
              std::string(reinterpret_cast<const char*>(value.payload.data()) + 2,
                          value.payload.size() - 2),
              200);
        }
        closed_ = true;
        return message;
      }
      case kOpcodeText:
      case kOpcodeBinary:
        if (in_message) return ProtocolError("a new message started before the previous ended");
        message_opcode = value.opcode;
        in_message = true;
        payload = std::move(value.payload);
        break;
      case kOpcodeContinuation:
        if (!in_message) return ProtocolError("continuation frame without a message");
        if (payload.size() + value.payload.size() > options_.max_message_bytes) {
          return Status::Error(ErrorKind::kProtocolError,
                               "websocket: message exceeds the size limit");
        }
        payload.insert(payload.end(), value.payload.begin(), value.payload.end());
        break;
      default:
        return ProtocolError("unknown opcode");
    }

    if (value.fin) {
      WebSocketMessage message;
      message.kind = message_opcode == kOpcodeText ? WebSocketMessage::Kind::kText
                                                   : WebSocketMessage::Kind::kBinary;
      message.payload = std::move(payload);
      return message;
    }
    if (payload.size() > options_.max_message_bytes) {
      return Status::Error(ErrorKind::kProtocolError, "websocket: message exceeds the size limit");
    }
  }
}

Status WebSocketClient::SendFrame(std::uint8_t opcode, ByteView payload) {
  if (!transport_) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "websocket: not connected");
  }
  std::uint8_t mask[4];
  if (!SecureRandomBytes(mask, sizeof(mask))) {
    return Status::Error(ErrorKind::kInternal, "websocket: no secure random source for masking");
  }

  Bytes frame;
  frame.reserve(payload.size() + 14);
  frame.push_back(static_cast<std::uint8_t>(0x80 | opcode));  // always FIN
  std::size_t length = payload.size();
  if (length < 126) {
    frame.push_back(static_cast<std::uint8_t>(0x80 | length));
  } else if (length <= 0xFFFF) {
    frame.push_back(static_cast<std::uint8_t>(0x80 | 126));
    frame.push_back(static_cast<std::uint8_t>((length >> 8) & 0xFF));
    frame.push_back(static_cast<std::uint8_t>(length & 0xFF));
  } else {
    frame.push_back(static_cast<std::uint8_t>(0x80 | 127));
    for (int i = 7; i >= 0; --i) {
      frame.push_back(static_cast<std::uint8_t>((static_cast<std::uint64_t>(length) >>
                                                 (8 * static_cast<unsigned>(i))) &
                                                0xFF));
    }
  }
  frame.insert(frame.end(), mask, mask + 4);
  std::size_t body_offset = frame.size();
  frame.resize(body_offset + length);
  for (std::size_t i = 0; i < length; ++i) {
    frame[body_offset + i] = static_cast<std::uint8_t>(payload[i] ^ mask[i % 4]);
  }

  std::lock_guard<std::mutex> lock(write_mutex_);
  return transport_->WriteAll(ByteView(frame), 30000);
}

Status WebSocketClient::SendText(const std::string& text) {
  return SendFrame(kOpcodeText, ByteView(text));
}

Status WebSocketClient::SendBinary(ByteView payload) {
  return SendFrame(kOpcodeBinary, payload);
}

Status WebSocketClient::SendPing(ByteView payload) { return SendFrame(kOpcodePing, payload); }

Status WebSocketClient::SendPong(ByteView payload) { return SendFrame(kOpcodePong, payload); }

Status WebSocketClient::SendClose(std::uint16_t code, const std::string& reason) {
  Bytes payload;
  payload.push_back(static_cast<std::uint8_t>((code >> 8) & 0xFF));
  payload.push_back(static_cast<std::uint8_t>(code & 0xFF));
  std::string trimmed = reason.size() > 120 ? reason.substr(0, 120) : reason;
  payload.insert(payload.end(), trimmed.begin(), trimmed.end());
  Status status = SendFrame(kOpcodeClose, ByteView(payload));
  closed_ = true;
  return status;
}

void WebSocketClient::SetInterrupt(Notifier* notifier) {
  interrupt_ = notifier;
  if (transport_) transport_->SetInterrupt(notifier);
}

void WebSocketClient::Cancel() {
  if (cancel_) cancel_->Cancel();
  if (transport_) transport_->Cancel();
}

void WebSocketClient::Close() {
  closed_ = true;
  if (transport_) {
    transport_->Close();
    transport_.reset();
  }
}

}  // namespace net
}  // namespace tmirror
