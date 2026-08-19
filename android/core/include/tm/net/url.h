#pragma once

#include <cstdint>
#include <string>

#include "tm/util/result.h"

namespace tmirror {
namespace net {

struct Url {
  std::string scheme;  // "https", "wss", "http", "ws"
  std::string host;
  std::uint16_t port = 0;
  std::string path = "/";
  std::string query;

  bool secure() const { return scheme == "https" || scheme == "wss"; }
  /// scheme://host[:port] — the form the relay binds into signing inputs.
  std::string origin() const;
  /// path plus query, as it appears on the request line.
  std::string request_target() const;
  std::string ToString() const;
};

/// Strict URL parser. Rejects credentials in the URL, fragments, non-ASCII hosts and
/// anything it does not fully understand rather than guessing: this value decides
/// which host a token is sent to.
Result<Url> ParseUrl(const std::string& text);

/// Resolve a relative path against a base URL, preserving scheme/host/port.
Url WithPath(const Url& base, const std::string& path);

/// Convert an http(s) base to its ws(s) equivalent.
Url ToWebSocketUrl(const Url& base);

}  // namespace net
}  // namespace tmirror
