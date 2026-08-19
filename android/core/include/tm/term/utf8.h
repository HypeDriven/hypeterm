#pragma once

#include <cstdint>
#include <string>

#include "tm/util/bytes.h"

namespace tmirror {
namespace term {

constexpr char32_t kReplacementChar = 0xFFFD;

/// Incremental UTF-8 decoder.
///
/// PTY bytes arrive in arbitrary chunks, so a multi-byte sequence may be split across
/// any boundary, including between every pair of its bytes (spec §8.1, §16.1). State
/// therefore lives in the decoder, not on the stack of whoever calls it.
///
/// Malformed input follows the Unicode "maximal subpart" recommendation: each
/// ill-formed subsequence produces exactly one U+FFFD and the byte that ended it is
/// re-examined, so a truncated sequence followed by valid ASCII never eats the ASCII.
class Utf8Decoder {
 public:
  /// Result of feeding one byte.
  enum class Step {
    kIncomplete,  // more bytes needed
    kEmit,        // `code_point` is a decoded scalar value
    kEmitTwice,   // an error replacement, then re-process this byte (see `Feed`)
  };

  void Reset();

  /// Feed one byte. When it returns true, `*out` holds a code point to emit; when
  /// `*reprocess` is set the same byte must be fed again after handling `*out`.
  bool Feed(std::uint8_t byte, char32_t* out, bool* reprocess);

  /// Flush a partial sequence at end of stream (or before a hard reset): emits one
  /// replacement character if a sequence was in progress.
  bool Flush(char32_t* out);

  bool has_partial_sequence() const { return remaining_ > 0; }

 private:
  std::uint32_t accumulator_ = 0;
  int remaining_ = 0;   // continuation bytes still expected
  int total_ = 0;       // total length of the sequence in progress
  std::uint8_t lower_bound_ = 0x80;  // valid range for the next continuation byte
  std::uint8_t upper_bound_ = 0xBF;
};

/// Convenience for tests and for encoding text back out (IME commits, paste).
void AppendUtf8(char32_t code_point, std::string* out);
std::string EncodeUtf8(char32_t code_point);
/// Decodes a whole buffer, replacing malformed sequences. Used for control-message
/// text, never for the terminal stream (which must be decoded incrementally).
std::u32string DecodeUtf8Lossy(ByteView bytes);
std::string EncodeUtf8(const std::u32string& text);

}  // namespace term
}  // namespace tmirror
