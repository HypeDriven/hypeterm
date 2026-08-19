#include "tm/net/http_client.h"

#include <algorithm>

#include "tm/util/log.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace net {
namespace {

constexpr const char kTag[] = "tm.http";

Status ProtocolError(const std::string& message) {
  return Status::Error(ErrorKind::kProtocolError, "http: " + message);
}

}  // namespace

std::string HttpResponse::Header(const std::string& name) const {
  for (const HttpHeader& header : headers) {
    if (EqualsIgnoreCaseAscii(header.name, name)) return header.value;
  }
  return std::string();
}

HttpConnection::~HttpConnection() {
  if (transport_) transport_->Close();
}

Result<std::unique_ptr<HttpConnection>> HttpConnection::Open(
    const HttpClientConfig& config, std::shared_ptr<CancelSignal> cancel) {
  TlsConfig tls = config.tls;
  if (tls.hostname.empty()) tls.hostname = config.host;
  TransportOptions options;
  options.connect_timeout_ms = config.connect_timeout_ms;
  options.dialer = config.dialer;
  options.allow_cleartext_over_tunnel = config.allow_cleartext_over_tunnel;
  Result<std::unique_ptr<Transport>> transport =
      OpenTransport(config.scheme, config.host, config.port, tls, std::move(cancel), options);
  if (!transport.ok()) return transport.status();

  auto connection = std::unique_ptr<HttpConnection>(new HttpConnection());
  connection->transport_ = transport.take();
  return connection;
}

Status HttpConnection::WriteRequest(const HttpRequest& request, const HttpClientConfig& config,
                                    Millis timeout_ms) {
  std::string head;
  head.reserve(256 + request.headers.size() * 48);
  head += request.method;
  head += " ";
  head += request.target;
  head += " HTTP/1.1\r\n";
  head += "Host: ";
  head += config.host;
  bool default_port = (config.scheme == "https" || config.scheme == "wss") ? config.port == 443
                                                                          : config.port == 80;
  if (!default_port) {
    head += ":";
    head += Uint64ToString(config.port);
  }
  head += "\r\n";
  head += "User-Agent: ";
  head += config.user_agent;
  head += "\r\n";

  bool has_content_type = false;
  bool has_connection = false;
  for (const HttpHeader& header : request.headers) {
    if (EqualsIgnoreCaseAscii(header.name, "content-type")) has_content_type = true;
    if (EqualsIgnoreCaseAscii(header.name, "connection")) has_connection = true;
    // A header value containing CR or LF would allow request splitting.
    if (header.value.find('\r') != std::string::npos ||
        header.value.find('\n') != std::string::npos) {
      return Status::Error(ErrorKind::kInvalidArgument, "http: illegal header value");
    }
    head += header.name;
    head += ": ";
    head += header.value;
    head += "\r\n";
  }
  if (!request.body.empty() && !has_content_type) {
    head += "Content-Type: ";
    head += request.content_type.empty() ? "application/json" : request.content_type;
    head += "\r\n";
  }
  if (!has_connection) head += "Connection: close\r\n";
  head += "Content-Length: ";
  head += Uint64ToString(request.body.size());
  head += "\r\n\r\n";

  Status status = transport_->WriteAll(ByteView(head), timeout_ms);
  if (!status.ok()) return status;
  if (!request.body.empty()) {
    status = transport_->WriteAll(ByteView(request.body), timeout_ms);
  }
  return status;
}

Status HttpConnection::FillBuffer(Millis timeout_ms) {
  if (eof_) return Status::Error(ErrorKind::kProtocolError, "http: unexpected end of response");
  if (buffer_position_ > 0 && buffer_position_ == buffer_.size()) {
    buffer_.clear();
    buffer_position_ = 0;
  }
  std::uint8_t chunk[8192];
  Result<std::size_t> read = transport_->Read(chunk, sizeof(chunk), timeout_ms);
  if (!read.ok()) return read.status();
  if (read.value() == 0) {
    eof_ = true;
    return Status::Error(ErrorKind::kProtocolError, "http: connection closed by the server");
  }
  buffer_.insert(buffer_.end(), chunk, chunk + read.value());
  return Status::Ok();
}

Result<std::string> HttpConnection::ReadLine(Millis timeout_ms, std::size_t max_length) {
  while (true) {
    for (std::size_t i = buffer_position_; i < buffer_.size(); ++i) {
      if (buffer_[i] == '\n') {
        std::size_t end = i;
        if (end > buffer_position_ && buffer_[end - 1] == '\r') --end;
        std::string line(reinterpret_cast<const char*>(buffer_.data() + buffer_position_),
                         end - buffer_position_);
        buffer_position_ = i + 1;
        return line;
      }
    }
    if (buffer_.size() - buffer_position_ > max_length) {
      return ProtocolError("header line exceeds the limit");
    }
    Status status = FillBuffer(timeout_ms);
    if (!status.ok()) return status;
  }
}

Result<HttpResponse> HttpConnection::ReadResponseHead(Millis timeout_ms,
                                                      std::size_t max_header_bytes) {
  HttpResponse response;
  Result<std::string> status_line = ReadLine(timeout_ms, max_header_bytes);
  if (!status_line.ok()) return status_line.status();

  const std::string& line = status_line.value();
  if (!StartsWith(line, "HTTP/1.")) return ProtocolError("malformed status line");
  std::size_t first_space = line.find(' ');
  if (first_space == std::string::npos) return ProtocolError("malformed status line");
  std::size_t code_end = line.find(' ', first_space + 1);
  std::string code =
      line.substr(first_space + 1, code_end == std::string::npos ? std::string::npos
                                                                 : code_end - first_space - 1);
  std::uint64_t status_code = 0;
  if (!ParseUint64(Trim(code), 999, &status_code)) return ProtocolError("malformed status code");
  response.status = static_cast<int>(status_code);

  std::size_t consumed = line.size();
  while (true) {
    Result<std::string> header_line = ReadLine(timeout_ms, max_header_bytes);
    if (!header_line.ok()) return header_line.status();
    if (header_line.value().empty()) break;
    consumed += header_line.value().size();
    if (consumed > max_header_bytes) return ProtocolError("response headers exceed the limit");
    if (response.headers.size() >= 128) return ProtocolError("too many response headers");

    std::size_t colon = header_line.value().find(':');
    if (colon == std::string::npos) return ProtocolError("malformed header");
    HttpHeader header;
    header.name = Trim(header_line.value().substr(0, colon));
    header.value = Trim(header_line.value().substr(colon + 1));
    if (header.name.empty()) return ProtocolError("malformed header name");
    response.headers.push_back(std::move(header));
  }
  return response;
}

Status HttpConnection::ReadBody(HttpResponse* response, Millis timeout_ms,
                                std::size_t max_bytes) {
  std::string encoding = ToLowerAscii(response->Header("transfer-encoding"));
  if (encoding.find("chunked") != std::string::npos) {
    while (true) {
      Result<std::string> size_line = ReadLine(timeout_ms, 64);
      if (!size_line.ok()) return size_line.status();
      std::string size_text = size_line.value();
      std::size_t semicolon = size_text.find(';');
      if (semicolon != std::string::npos) size_text = size_text.substr(0, semicolon);
      size_text = Trim(size_text);
      std::size_t chunk_size = 0;
      if (size_text.empty() || size_text.size() > 8) return ProtocolError("bad chunk size");
      for (char c : size_text) {
        int digit;
        if (c >= '0' && c <= '9') digit = c - '0';
        else if (c >= 'a' && c <= 'f') digit = c - 'a' + 10;
        else if (c >= 'A' && c <= 'F') digit = c - 'A' + 10;
        else return ProtocolError("bad chunk size");
        chunk_size = chunk_size * 16 + static_cast<std::size_t>(digit);
      }
      if (chunk_size == 0) {
        // Trailer section, then done.
        while (true) {
          Result<std::string> trailer = ReadLine(timeout_ms, 1024);
          if (!trailer.ok()) return trailer.status();
          if (trailer.value().empty()) break;
        }
        return Status::Ok();
      }
      if (response->body.size() + chunk_size > max_bytes) {
        return Status::Error(ErrorKind::kProtocolError, "http: response body exceeds the limit");
      }
      while (buffer_.size() - buffer_position_ < chunk_size) {
        Status status = FillBuffer(timeout_ms);
        if (!status.ok()) return status;
      }
      response->body.append(reinterpret_cast<const char*>(buffer_.data() + buffer_position_),
                            chunk_size);
      buffer_position_ += chunk_size;
      Result<std::string> terminator = ReadLine(timeout_ms, 8);
      if (!terminator.ok()) return terminator.status();
    }
  }

  std::string length_header = response->Header("content-length");
  if (!length_header.empty()) {
    std::uint64_t length = 0;
    if (!ParseUint64(Trim(length_header), max_bytes, &length)) {
      return Status::Error(ErrorKind::kProtocolError,
                           "http: missing or oversized content-length");
    }
    while (buffer_.size() - buffer_position_ < length) {
      Status status = FillBuffer(timeout_ms);
      if (!status.ok()) return status;
    }
    response->body.assign(reinterpret_cast<const char*>(buffer_.data() + buffer_position_),
                          static_cast<std::size_t>(length));
    buffer_position_ += static_cast<std::size_t>(length);
    return Status::Ok();
  }

  // No length and no chunking: read until close, still bounded.
  while (true) {
    Status status = FillBuffer(timeout_ms);
    if (!status.ok()) {
      if (eof_) break;
      return status;
    }
    if (buffer_.size() - buffer_position_ > max_bytes) {
      return Status::Error(ErrorKind::kProtocolError, "http: response body exceeds the limit");
    }
  }
  response->body.assign(reinterpret_cast<const char*>(buffer_.data() + buffer_position_),
                        buffer_.size() - buffer_position_);
  buffer_position_ = buffer_.size();
  return Status::Ok();
}

Bytes HttpConnection::TakeBuffered() {
  Bytes remaining(buffer_.begin() + static_cast<std::ptrdiff_t>(buffer_position_), buffer_.end());
  buffer_.clear();
  buffer_position_ = 0;
  return remaining;
}

Result<HttpResponse> HttpClient::Send(const HttpRequest& request,
                                      std::shared_ptr<CancelSignal> cancel) {
  Result<std::unique_ptr<HttpConnection>> connection = HttpConnection::Open(config_, cancel);
  if (!connection.ok()) return connection.status();

  HttpConnection* http = connection.value().get();
  Status status = http->WriteRequest(request, config_, config_.request_timeout_ms);
  if (!status.ok()) return status;

  Result<HttpResponse> response =
      http->ReadResponseHead(config_.request_timeout_ms, config_.max_header_bytes);
  if (!response.ok()) return response.status();

  status = http->ReadBody(&response.value(), config_.request_timeout_ms,
                          config_.max_response_bytes);
  if (!status.ok()) return status;

  TM_LOG_DEBUG(kTag, "%s %s -> %d (%zu body bytes)", request.method.c_str(),
               request.target.c_str(), response.value().status, response.value().body.size());
  return response;
}

}  // namespace net
}  // namespace tmirror
