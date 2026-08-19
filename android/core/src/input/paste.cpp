#include "tm/input/paste.h"

#include <cstring>

namespace tmirror {
namespace input {

std::string Paste::Normalize(const std::string& utf8_text, const Options& options) {
  std::string out;
  out.reserve(utf8_text.size());
  for (std::size_t i = 0; i < utf8_text.size(); ++i) {
    unsigned char c = static_cast<unsigned char>(utf8_text[i]);
    if (c == '\r') {
      out.push_back('\r');
      if (i + 1 < utf8_text.size() && utf8_text[i + 1] == '\n') ++i;
      continue;
    }
    if (c == '\n') {
      out.push_back('\r');
      continue;
    }
    if (c == '\t') {
      out.push_back('\t');
      continue;
    }
    if (c < 0x20 || c == 0x7F) {
      if (options.bracketed || !options.strip_controls_when_unbracketed) {
        out.push_back(static_cast<char>(c));
      }
      // Otherwise dropped: an unbracketed paste containing ESC would be executed by
      // the remote application rather than inserted.
      continue;
    }
    out.push_back(static_cast<char>(c));
  }
  return out;
}

std::vector<std::string> Paste::Prepare(const std::string& utf8_text, const Options& options,
                                        bool* too_large) {
  if (too_large != nullptr) *too_large = false;
  std::vector<std::string> chunks;
  if (utf8_text.empty()) return chunks;
  if (utf8_text.size() > options.max_bytes) {
    if (too_large != nullptr) *too_large = true;
    return chunks;
  }

  std::string normalized = Normalize(utf8_text, options);
  if (normalized.empty()) return chunks;

  std::size_t chunk_bytes = options.chunk_bytes == 0 ? 4096 : options.chunk_bytes;

  if (options.bracketed) chunks.emplace_back(kBracketStart);

  std::size_t position = 0;
  while (position < normalized.size()) {
    std::size_t length = chunk_bytes;
    if (position + length > normalized.size()) {
      length = normalized.size() - position;
    } else {
      // Never split a UTF-8 sequence: back up to a code-point boundary.
      while (length > 1 &&
             (static_cast<unsigned char>(normalized[position + length]) & 0xC0) == 0x80) {
        --length;
      }
    }
    chunks.emplace_back(normalized, position, length);
    position += length;
  }

  if (options.bracketed) chunks.emplace_back(kBracketEnd);
  return chunks;
}

}  // namespace input
}  // namespace tmirror
