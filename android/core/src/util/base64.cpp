#include "tm/util/base64.h"

namespace tmirror {
namespace {

constexpr char kUrlAlphabet[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
constexpr char kStdAlphabet[] =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

std::string Encode(ByteView bytes, const char* alphabet, bool pad) {
  std::string out;
  out.reserve(((bytes.size() + 2) / 3) * 4);
  std::size_t i = 0;
  while (i + 3 <= bytes.size()) {
    std::uint32_t v = (static_cast<std::uint32_t>(bytes[i]) << 16) |
                      (static_cast<std::uint32_t>(bytes[i + 1]) << 8) |
                      static_cast<std::uint32_t>(bytes[i + 2]);
    out.push_back(alphabet[(v >> 18) & 0x3F]);
    out.push_back(alphabet[(v >> 12) & 0x3F]);
    out.push_back(alphabet[(v >> 6) & 0x3F]);
    out.push_back(alphabet[v & 0x3F]);
    i += 3;
  }
  std::size_t remaining = bytes.size() - i;
  if (remaining == 1) {
    std::uint32_t v = static_cast<std::uint32_t>(bytes[i]) << 16;
    out.push_back(alphabet[(v >> 18) & 0x3F]);
    out.push_back(alphabet[(v >> 12) & 0x3F]);
    if (pad) {
      out.push_back('=');
      out.push_back('=');
    }
  } else if (remaining == 2) {
    std::uint32_t v = (static_cast<std::uint32_t>(bytes[i]) << 16) |
                      (static_cast<std::uint32_t>(bytes[i + 1]) << 8);
    out.push_back(alphabet[(v >> 18) & 0x3F]);
    out.push_back(alphabet[(v >> 12) & 0x3F]);
    out.push_back(alphabet[(v >> 6) & 0x3F]);
    if (pad) out.push_back('=');
  }
  return out;
}

int DecodeChar(char c, bool url) {
  if (c >= 'A' && c <= 'Z') return c - 'A';
  if (c >= 'a' && c <= 'z') return c - 'a' + 26;
  if (c >= '0' && c <= '9') return c - '0' + 52;
  if (url) {
    if (c == '-') return 62;
    if (c == '_') return 63;
  } else {
    if (c == '+') return 62;
    if (c == '/') return 63;
  }
  return -1;
}

bool Decode(const std::string& text, Bytes* out, bool url) {
  out->clear();
  std::size_t length = text.size();
  // Tolerate padding on input even when the canonical form omits it.
  while (length > 0 && text[length - 1] == '=') --length;
  if (length % 4 == 1) return false;

  std::uint32_t accumulator = 0;
  int bits = 0;
  out->reserve(length * 3 / 4);
  for (std::size_t i = 0; i < length; ++i) {
    int value = DecodeChar(text[i], url);
    if (value < 0) return false;
    accumulator = (accumulator << 6) | static_cast<std::uint32_t>(value);
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out->push_back(static_cast<std::uint8_t>((accumulator >> bits) & 0xFF));
    }
  }
  // Leftover bits must be zero, otherwise the encoding was not canonical.
  if (bits > 0 && (accumulator & ((1u << bits) - 1)) != 0) return false;
  return true;
}

}  // namespace

std::string Base64UrlEncode(ByteView bytes) { return Encode(bytes, kUrlAlphabet, false); }
bool Base64UrlDecode(const std::string& text, Bytes* out) { return Decode(text, out, true); }
std::string Base64Encode(ByteView bytes) { return Encode(bytes, kStdAlphabet, true); }
bool Base64Decode(const std::string& text, Bytes* out) { return Decode(text, out, false); }

}  // namespace tmirror
