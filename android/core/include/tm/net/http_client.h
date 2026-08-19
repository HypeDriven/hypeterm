#pragma once

#include <memory>
#include <string>
#include <vector>

#include "tm/net/tls.h"
#include "tm/net/url.h"

namespace tmirror {
namespace net {

struct HttpHeader {
  std::string name;
  std::string value;
};

struct HttpRequest {
  std::string method = "GET";
  std::string target = "/";
  std::vector<HttpHeader> headers;
  std::string body;
  std::string content_type;
};

struct HttpResponse {
  int status = 0;
  std::vector<HttpHeader> headers;
  std::string body;

  /// Case-insensitive lookup; empty when absent.
  std::string Header(const std::string& name) const;
  bool ok() const { return status >= 200 && status < 300; }
};

struct HttpClientConfig {
  std::string scheme = "https";
  std::string host;
  std::uint16_t port = 443;
  TlsConfig tls;
  Millis connect_timeout_ms = 15000;
  Millis request_timeout_ms = 30000;
  /// Bounded because the body is untrusted input (spec §7.4).
  std::size_t max_response_bytes = 1024 * 1024;
  std::size_t max_header_bytes = 64 * 1024;
  std::string user_agent = "TerminalMirror/0.1";

  /// Optional tunnel to dial through, and whether cleartext may cross it. Not owned.
  Dialer* dialer = nullptr;
  bool allow_cleartext_over_tunnel = false;
};

/// One HTTP/1.1 connection. Also the substrate for the WebSocket handshake, which
/// needs the same request writing and header parsing but then keeps the socket.
class HttpConnection {
 public:
  ~HttpConnection();

  static Result<std::unique_ptr<HttpConnection>> Open(const HttpClientConfig& config,
                                                      std::shared_ptr<CancelSignal> cancel);

  Status WriteRequest(const HttpRequest& request, const HttpClientConfig& config,
                      Millis timeout_ms);
  /// Reads the status line and headers only.
  Result<HttpResponse> ReadResponseHead(Millis timeout_ms, std::size_t max_header_bytes);
  /// Reads the body according to Content-Length or chunked transfer coding.
  Status ReadBody(HttpResponse* response, Millis timeout_ms, std::size_t max_bytes);

  Transport* transport() { return transport_.get(); }
  std::unique_ptr<Transport> TakeTransport() { return std::move(transport_); }
  /// Bytes already read past the response head, which the WebSocket framer must
  /// consume before reading from the socket again.
  Bytes TakeBuffered();

 private:
  HttpConnection() = default;
  Result<std::string> ReadLine(Millis timeout_ms, std::size_t max_length);
  Status FillBuffer(Millis timeout_ms);

  std::unique_ptr<Transport> transport_;
  Bytes buffer_;
  std::size_t buffer_position_ = 0;
  bool eof_ = false;
};

/// Single-request client (spec §7.1: registration, authentication, discovery).
class HttpClient {
 public:
  explicit HttpClient(HttpClientConfig config) : config_(std::move(config)) {}

  Result<HttpResponse> Send(const HttpRequest& request,
                            std::shared_ptr<CancelSignal> cancel = nullptr);

  const HttpClientConfig& config() const { return config_; }
  HttpClientConfig& mutable_config() { return config_; }

 private:
  HttpClientConfig config_;
};

}  // namespace net
}  // namespace tmirror
