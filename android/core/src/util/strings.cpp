#include "tm/util/strings.h"

#include <cctype>
#include <cstdio>
#include <cstring>

#include "tm/util/result.h"

namespace tmirror {

void SecureZero(void* data, std::size_t size) {
  if (data == nullptr || size == 0) return;
  volatile std::uint8_t* p = static_cast<volatile std::uint8_t*>(data);
  while (size-- > 0) *p++ = 0;
}

const char* ErrorKindName(ErrorKind kind) {
  switch (kind) {
    case ErrorKind::kNone: return "ok";
    case ErrorKind::kNetworkUnavailable: return "network_unavailable";
    case ErrorKind::kTlsFailure: return "tls_failure";
    case ErrorKind::kAuthFailed: return "auth_failed";
    case ErrorKind::kAuthExpired: return "auth_expired";
    case ErrorKind::kPermissionDenied: return "permission_denied";
    case ErrorKind::kNotFound: return "not_found";
    case ErrorKind::kTerminalClosed: return "terminal_closed";
    case ErrorKind::kProtocolIncompatible: return "protocol_incompatible";
    case ErrorKind::kProtocolError: return "protocol_error";
    case ErrorKind::kSyncFailure: return "sync_failure";
    case ErrorKind::kInputRefused: return "input_refused";
    case ErrorKind::kInputUndeliverable: return "input_undeliverable";
    case ErrorKind::kRateLimited: return "rate_limited";
    case ErrorKind::kServerError: return "server_error";
    case ErrorKind::kStorageError: return "storage_error";
    case ErrorKind::kCancelled: return "cancelled";
    case ErrorKind::kTimeout: return "timeout";
    case ErrorKind::kInvalidArgument: return "invalid_argument";
    case ErrorKind::kInternal: return "internal";
  }
  return "unknown";
}

bool ErrorKindIsRecoverable(ErrorKind kind) {
  switch (kind) {
    case ErrorKind::kNetworkUnavailable:
    case ErrorKind::kTimeout:
    case ErrorKind::kRateLimited:
    case ErrorKind::kServerError:
    case ErrorKind::kSyncFailure:
    case ErrorKind::kAuthExpired:
    case ErrorKind::kInputUndeliverable:
      return true;
    default:
      return false;
  }
}

std::string Status::ToString() const {
  std::string out = ErrorKindName(kind_);
  if (!code_.empty()) {
    out += "[";
    out += code_;
    out += "]";
  }
  if (!message_.empty()) {
    out += ": ";
    out += message_;
  }
  return out;
}

std::string ToLowerAscii(const std::string& s) {
  std::string out = s;
  for (char& c : out) {
    if (c >= 'A' && c <= 'Z') c = static_cast<char>(c - 'A' + 'a');
  }
  return out;
}

bool EqualsIgnoreCaseAscii(const std::string& a, const std::string& b) {
  if (a.size() != b.size()) return false;
  for (std::size_t i = 0; i < a.size(); ++i) {
    char ca = a[i], cb = b[i];
    if (ca >= 'A' && ca <= 'Z') ca = static_cast<char>(ca - 'A' + 'a');
    if (cb >= 'A' && cb <= 'Z') cb = static_cast<char>(cb - 'A' + 'a');
    if (ca != cb) return false;
  }
  return true;
}

bool StartsWith(const std::string& s, const std::string& prefix) {
  return s.size() >= prefix.size() && s.compare(0, prefix.size(), prefix) == 0;
}

bool EndsWith(const std::string& s, const std::string& suffix) {
  return s.size() >= suffix.size() &&
         s.compare(s.size() - suffix.size(), suffix.size(), suffix) == 0;
}

std::string Trim(const std::string& s) {
  std::size_t begin = 0;
  std::size_t end = s.size();
  auto is_space = [](char c) {
    return c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\f' || c == '\v';
  };
  while (begin < end && is_space(s[begin])) ++begin;
  while (end > begin && is_space(s[end - 1])) --end;
  return s.substr(begin, end - begin);
}

std::vector<std::string> Split(const std::string& s, char delimiter) {
  std::vector<std::string> parts;
  std::string current;
  for (char c : s) {
    if (c == delimiter) {
      parts.push_back(current);
      current.clear();
    } else {
      current.push_back(c);
    }
  }
  parts.push_back(current);
  return parts;
}

std::string Join(const std::vector<std::string>& parts, const std::string& sep) {
  std::string out;
  for (std::size_t i = 0; i < parts.size(); ++i) {
    if (i != 0) out += sep;
    out += parts[i];
  }
  return out;
}

std::string HexEncode(ByteView bytes) {
  static const char kDigits[] = "0123456789abcdef";
  std::string out;
  out.reserve(bytes.size() * 2);
  for (std::size_t i = 0; i < bytes.size(); ++i) {
    out.push_back(kDigits[bytes[i] >> 4]);
    out.push_back(kDigits[bytes[i] & 0x0F]);
  }
  return out;
}

static int HexValue(char c) {
  if (c >= '0' && c <= '9') return c - '0';
  if (c >= 'a' && c <= 'f') return c - 'a' + 10;
  if (c >= 'A' && c <= 'F') return c - 'A' + 10;
  return -1;
}

bool HexDecode(const std::string& hex, Bytes* out) {
  if (hex.size() % 2 != 0) return false;
  out->clear();
  out->reserve(hex.size() / 2);
  for (std::size_t i = 0; i < hex.size(); i += 2) {
    int hi = HexValue(hex[i]);
    int lo = HexValue(hex[i + 1]);
    if (hi < 0 || lo < 0) return false;
    out->push_back(static_cast<std::uint8_t>((hi << 4) | lo));
  }
  return true;
}

std::string UrlEncode(const std::string& s) {
  static const char kDigits[] = "0123456789ABCDEF";
  std::string out;
  for (char raw : s) {
    unsigned char c = static_cast<unsigned char>(raw);
    if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
        c == '-' || c == '_' || c == '.' || c == '~') {
      out.push_back(static_cast<char>(c));
    } else {
      out.push_back('%');
      out.push_back(kDigits[c >> 4]);
      out.push_back(kDigits[c & 0x0F]);
    }
  }
  return out;
}

bool ParseUint64(const std::string& s, std::uint64_t max, std::uint64_t* out) {
  if (s.empty() || s.size() > 20) return false;
  std::uint64_t value = 0;
  for (char c : s) {
    if (c < '0' || c > '9') return false;
    std::uint64_t digit = static_cast<std::uint64_t>(c - '0');
    if (value > (UINT64_MAX - digit) / 10) return false;
    value = value * 10 + digit;
    if (value > max) return false;
  }
  *out = value;
  return true;
}

std::string Concat(std::initializer_list<std::string> parts) {
  std::size_t total = 0;
  for (const auto& p : parts) total += p.size();
  std::string out;
  out.reserve(total);
  for (const auto& p : parts) out += p;
  return out;
}

std::string Uint64ToString(std::uint64_t v) {
  char buf[24];
  int n = std::snprintf(buf, sizeof(buf), "%llu", static_cast<unsigned long long>(v));
  return std::string(buf, static_cast<std::size_t>(n < 0 ? 0 : n));
}

std::string Int64ToString(std::int64_t v) {
  char buf[24];
  int n = std::snprintf(buf, sizeof(buf), "%lld", static_cast<long long>(v));
  return std::string(buf, static_cast<std::size_t>(n < 0 ? 0 : n));
}

std::string SanitizeForMessage(const std::string& s, std::size_t max_length) {
  std::string out;
  out.reserve(s.size() < max_length ? s.size() : max_length);
  bool truncated = false;
  for (char c : s) {
    if (out.size() >= max_length) {
      truncated = true;
      break;
    }
    unsigned char u = static_cast<unsigned char>(c);
    if (u < 0x20 || u == 0x7F) {
      out.push_back('.');
    } else {
      out.push_back(c);
    }
  }
  if (truncated) {
    // A limit counted in bytes can fall inside a multi-byte character, and the half
    // sequence left behind travels on into a log line or across the JNI boundary, where
    // it is read as the beginning of something that is not there. Drop the final
    // character when the cut left it short — and only then, so an intact one survives.
    std::size_t start = out.size();
    while (start > 0 && (static_cast<unsigned char>(out[start - 1]) & 0xC0) == 0x80) {
      --start;
    }
    if (start > 0) {
      const unsigned char lead = static_cast<unsigned char>(out[start - 1]);
      std::size_t expected = 1;
      if ((lead & 0xF8) == 0xF0) {
        expected = 4;
      } else if ((lead & 0xF0) == 0xE0) {
        expected = 3;
      } else if ((lead & 0xE0) == 0xC0) {
        expected = 2;
      }
      if (expected > 1 && out.size() - (start - 1) < expected) out.resize(start - 1);
    }
    out += "...";
  }
  return out;
}

}  // namespace tmirror
