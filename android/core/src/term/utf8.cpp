#include "tm/term/utf8.h"

namespace tmirror {
namespace term {

void Utf8Decoder::Reset() {
  accumulator_ = 0;
  remaining_ = 0;
  total_ = 0;
  lower_bound_ = 0x80;
  upper_bound_ = 0xBF;
}

bool Utf8Decoder::Feed(std::uint8_t byte, char32_t* out, bool* reprocess) {
  *reprocess = false;

  if (remaining_ == 0) {
    if (byte < 0x80) {
      *out = byte;
      return true;
    }
    if (byte >= 0xC2 && byte <= 0xDF) {
      accumulator_ = byte & 0x1Fu;
      remaining_ = 1;
      total_ = 2;
      lower_bound_ = 0x80;
      upper_bound_ = 0xBF;
      return false;
    }
    if (byte >= 0xE0 && byte <= 0xEF) {
      accumulator_ = byte & 0x0Fu;
      remaining_ = 2;
      total_ = 3;
      // Exclude overlong forms and the surrogate range at the first continuation.
      if (byte == 0xE0) {
        lower_bound_ = 0xA0;
        upper_bound_ = 0xBF;
      } else if (byte == 0xED) {
        lower_bound_ = 0x80;
        upper_bound_ = 0x9F;
      } else {
        lower_bound_ = 0x80;
        upper_bound_ = 0xBF;
      }
      return false;
    }
    if (byte >= 0xF0 && byte <= 0xF4) {
      accumulator_ = byte & 0x07u;
      remaining_ = 3;
      total_ = 4;
      if (byte == 0xF0) {
        lower_bound_ = 0x90;
        upper_bound_ = 0xBF;
      } else if (byte == 0xF4) {
        lower_bound_ = 0x80;
        upper_bound_ = 0x8F;
      } else {
        lower_bound_ = 0x80;
        upper_bound_ = 0xBF;
      }
      return false;
    }
    // 0x80-0xC1 and 0xF5-0xFF can never start a well-formed sequence. In a UTF-8
    // stream this also covers 8-bit C1 controls, which the emulator deliberately
    // does not support (spec §8.1 targets a UTF-8 xterm-256color profile).
    *out = kReplacementChar;
    return true;
  }

  if (byte < lower_bound_ || byte > upper_bound_) {
    // Ill-formed: emit one replacement for the maximal subpart consumed so far and
    // re-examine this byte, which may legitimately start a new sequence.
    Reset();
    *out = kReplacementChar;
    *reprocess = true;
    return true;
  }

  accumulator_ = (accumulator_ << 6) | (byte & 0x3Fu);
  lower_bound_ = 0x80;
  upper_bound_ = 0xBF;
  if (--remaining_ == 0) {
    char32_t code_point = static_cast<char32_t>(accumulator_);
    Reset();
    *out = code_point;
    return true;
  }
  return false;
}

bool Utf8Decoder::Flush(char32_t* out) {
  if (remaining_ == 0) return false;
  Reset();
  *out = kReplacementChar;
  return true;
}

void AppendUtf8(char32_t code_point, std::string* out) {
  std::uint32_t c = static_cast<std::uint32_t>(code_point);
  if (c > 0x10FFFF || (c >= 0xD800 && c <= 0xDFFF)) c = kReplacementChar;
  if (c < 0x80) {
    out->push_back(static_cast<char>(c));
  } else if (c < 0x800) {
    out->push_back(static_cast<char>(0xC0 | (c >> 6)));
    out->push_back(static_cast<char>(0x80 | (c & 0x3F)));
  } else if (c < 0x10000) {
    out->push_back(static_cast<char>(0xE0 | (c >> 12)));
    out->push_back(static_cast<char>(0x80 | ((c >> 6) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | (c & 0x3F)));
  } else {
    out->push_back(static_cast<char>(0xF0 | (c >> 18)));
    out->push_back(static_cast<char>(0x80 | ((c >> 12) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | ((c >> 6) & 0x3F)));
    out->push_back(static_cast<char>(0x80 | (c & 0x3F)));
  }
}

std::string EncodeUtf8(char32_t code_point) {
  std::string out;
  AppendUtf8(code_point, &out);
  return out;
}

std::u32string DecodeUtf8Lossy(ByteView bytes) {
  std::u32string out;
  Utf8Decoder decoder;
  for (std::size_t i = 0; i < bytes.size();) {
    char32_t code_point = 0;
    bool reprocess = false;
    if (decoder.Feed(bytes[i], &code_point, &reprocess)) {
      out.push_back(code_point);
      if (!reprocess) ++i;
    } else {
      ++i;
    }
  }
  char32_t tail = 0;
  if (decoder.Flush(&tail)) out.push_back(tail);
  return out;
}

std::string EncodeUtf8(const std::u32string& text) {
  std::string out;
  out.reserve(text.size());
  for (char32_t c : text) AppendUtf8(c, &out);
  return out;
}

}  // namespace term
}  // namespace tmirror
