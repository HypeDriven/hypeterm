#include "tm/net/url.h"

#include "tm/util/strings.h"

namespace tmirror {
namespace net {
namespace {

Status Invalid(const std::string& reason) {
  return Status::Error(ErrorKind::kInvalidArgument, "url: " + reason);
}

bool ValidHostChar(char c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
         c == '-' || c == '.' || c == '_' || c == ':' || c == '[' || c == ']';
}

}  // namespace

std::string Url::origin() const {
  std::string out = scheme + "://" + host;
  bool default_port = (secure() && port == 443) || (!secure() && port == 80);
  if (!default_port && port != 0) {
    out += ":";
    out += Uint64ToString(port);
  }
  return out;
}

std::string Url::request_target() const {
  std::string target = path.empty() ? "/" : path;
  if (!query.empty()) {
    target += "?";
    target += query;
  }
  return target;
}

std::string Url::ToString() const { return origin() + request_target(); }

Result<Url> ParseUrl(const std::string& text) {
  Url url;
  std::size_t scheme_end = text.find("://");
  if (scheme_end == std::string::npos) return Invalid("missing scheme");
  url.scheme = ToLowerAscii(text.substr(0, scheme_end));
  if (url.scheme != "http" && url.scheme != "https" && url.scheme != "ws" &&
      url.scheme != "wss") {
    return Invalid("unsupported scheme: " + SanitizeForMessage(url.scheme, 16));
  }

  std::size_t authority_begin = scheme_end + 3;
  std::size_t authority_end = text.size();
  for (std::size_t i = authority_begin; i < text.size(); ++i) {
    char c = text[i];
    if (c == '/' || c == '?' || c == '#') {
      authority_end = i;
      break;
    }
  }
  std::string authority = text.substr(authority_begin, authority_end - authority_begin);
  if (authority.empty()) return Invalid("missing host");
  if (authority.find('@') != std::string::npos) {
    // Credentials in a URL would end up in logs and in the origin string.
    return Invalid("credentials in the authority are not accepted");
  }

  std::string host = authority;
  std::string port_text;
  if (!authority.empty() && authority[0] == '[') {
    std::size_t close = authority.find(']');
    if (close == std::string::npos) return Invalid("malformed IPv6 literal");
    host = authority.substr(0, close + 1);
    if (close + 1 < authority.size()) {
      if (authority[close + 1] != ':') return Invalid("malformed authority");
      port_text = authority.substr(close + 2);
    }
  } else {
    std::size_t colon = authority.rfind(':');
    if (colon != std::string::npos) {
      host = authority.substr(0, colon);
      port_text = authority.substr(colon + 1);
    }
  }
  if (host.empty()) return Invalid("missing host");
  for (char c : host) {
    if (!ValidHostChar(c)) return Invalid("invalid character in host");
  }
  url.host = ToLowerAscii(host);

  if (port_text.empty()) {
    url.port = url.secure() ? 443 : 80;
  } else {
    std::uint64_t port = 0;
    if (!ParseUint64(port_text, 65535, &port) || port == 0) return Invalid("invalid port");
    url.port = static_cast<std::uint16_t>(port);
  }

  std::string remainder = text.substr(authority_end);
  std::size_t fragment = remainder.find('#');
  if (fragment != std::string::npos) remainder = remainder.substr(0, fragment);
  std::size_t question = remainder.find('?');
  if (question != std::string::npos) {
    url.path = remainder.substr(0, question);
    url.query = remainder.substr(question + 1);
  } else {
    url.path = remainder;
  }
  if (url.path.empty()) url.path = "/";
  if (url.path[0] != '/') return Invalid("path must be absolute");
  return url;
}

Url WithPath(const Url& base, const std::string& path) {
  Url url = base;
  url.path = path.empty() ? "/" : path;
  url.query.clear();
  return url;
}

Url ToWebSocketUrl(const Url& base) {
  Url url = base;
  if (url.scheme == "https") url.scheme = "wss";
  if (url.scheme == "http") url.scheme = "ws";
  return url;
}

}  // namespace net
}  // namespace tmirror
