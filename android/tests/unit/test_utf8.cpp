// UTF-8 decoding, spec §8.1 and §16.1: "incremental UTF-8 decoding across every
// possible input boundary".

#include <string>
#include <vector>

#include "framework.h"
#include "tm/term/utf8.h"
#include "tm/term/width.h"

using tmirror::ByteView;
using tmirror::term::AppendUtf8;
using tmirror::term::CharWidth;
using tmirror::term::DecodeUtf8Lossy;
using tmirror::term::EncodeUtf8;
using tmirror::term::kReplacementChar;
using tmirror::term::Utf8Decoder;

namespace {

/// Decodes `bytes` feeding them `chunk` at a time, exercising the resumable path.
std::u32string DecodeInChunks(const std::string& bytes, std::size_t chunk) {
  Utf8Decoder decoder;
  std::u32string out;
  std::size_t position = 0;
  while (position < bytes.size()) {
    std::size_t end = std::min(bytes.size(), position + chunk);
    while (position < end) {
      char32_t code_point = 0;
      bool reprocess = false;
      if (decoder.Feed(static_cast<std::uint8_t>(bytes[position]), &code_point, &reprocess)) {
        out.push_back(code_point);
        if (!reprocess) ++position;
      } else {
        ++position;
      }
    }
  }
  char32_t tail = 0;
  if (decoder.Flush(&tail)) out.push_back(tail);
  return out;
}

}  // namespace

TM_TEST(Utf8, DecodesAsciiAndMultibyte) {
  std::string input = "aé€𝄞";
  std::u32string expected = {U'a', 0xE9, 0x20AC, 0x1D11E};
  TM_CHECK_EQ(DecodeUtf8Lossy(ByteView(input)), expected);
}

TM_TEST(Utf8, SurvivesEveryChunkBoundary) {
  // Every boundary means every chunk size from 1 up to the whole buffer: a sequence
  // may be split between any two of its bytes.
  const std::string input = "aé€𝄞xÿ𐍈";
  const std::u32string expected = DecodeUtf8Lossy(ByteView(input));
  for (std::size_t chunk = 1; chunk <= input.size(); ++chunk) {
    TM_CHECK_MSG(DecodeInChunks(input, chunk) == expected,
                 "chunk size " + std::to_string(chunk));
  }
}

TM_TEST(Utf8, RejectsOverlongEncodings) {
  // C0 80 is an overlong NUL; each ill-formed byte yields one replacement.
  std::u32string decoded = DecodeUtf8Lossy(ByteView(std::string("\xC0\x80", 2)));
  TM_CHECK_EQ(decoded.size(), static_cast<std::size_t>(2));
  TM_CHECK_EQ(decoded[0], kReplacementChar);
  TM_CHECK_EQ(decoded[1], kReplacementChar);
}

TM_TEST(Utf8, RejectsSurrogatesAndOutOfRange) {
  // ED A0 80 encodes U+D800, which is not a scalar value.
  std::u32string surrogate = DecodeUtf8Lossy(ByteView(std::string("\xED\xA0\x80", 3)));
  for (char32_t c : surrogate) TM_CHECK_EQ(c, kReplacementChar);

  // F5 .. would encode beyond U+10FFFF.
  std::u32string over = DecodeUtf8Lossy(ByteView(std::string("\xF5\x80\x80\x80", 4)));
  for (char32_t c : over) TM_CHECK_EQ(c, kReplacementChar);
}

TM_TEST(Utf8, MaximalSubpartDoesNotEatTheNextCharacter) {
  // A truncated three-byte sequence followed by 'A': the replacement must not consume
  // the 'A', or valid text after a corrupt run would disappear.
  std::u32string decoded = DecodeUtf8Lossy(ByteView(std::string("\xE2\x82" "A", 3)));
  TM_CHECK_EQ(decoded.size(), static_cast<std::size_t>(2));
  TM_CHECK_EQ(decoded[0], kReplacementChar);
  TM_CHECK_EQ(decoded[1], U'A');
}

TM_TEST(Utf8, FlushEmitsReplacementForTruncatedTail) {
  Utf8Decoder decoder;
  char32_t code_point = 0;
  bool reprocess = false;
  TM_CHECK(!decoder.Feed(0xE2, &code_point, &reprocess));
  TM_CHECK(decoder.has_partial_sequence());
  TM_CHECK(decoder.Flush(&code_point));
  TM_CHECK_EQ(code_point, kReplacementChar);
  TM_CHECK(!decoder.Flush(&code_point));
}

TM_TEST(Utf8, RoundTripsEncoding) {
  const char32_t samples[] = {U'a', 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x10000, 0x10FFFF};
  for (char32_t sample : samples) {
    std::string encoded = EncodeUtf8(sample);
    std::u32string decoded = DecodeUtf8Lossy(ByteView(encoded));
    TM_CHECK_EQ(decoded.size(), static_cast<std::size_t>(1));
    TM_CHECK_EQ(decoded[0], sample);
  }
}

TM_TEST(Utf8, EncodingSubstitutesInvalidScalars) {
  std::string encoded;
  AppendUtf8(0xD800, &encoded);  // lone surrogate
  TM_CHECK_EQ(encoded, EncodeUtf8(kReplacementChar));
}

TM_TEST(Width, ClassifiesNarrowWideAndZeroWidth) {
  TM_CHECK_EQ(CharWidth(U'a'), 1);
  TM_CHECK_EQ(CharWidth(0x4E00), 2);   // CJK ideograph
  TM_CHECK_EQ(CharWidth(0xFF21), 2);   // fullwidth A
  TM_CHECK_EQ(CharWidth(0x0301), 0);   // combining acute
  TM_CHECK_EQ(CharWidth(0x200B), 0);   // zero-width space
  TM_CHECK_EQ(CharWidth(0x1F600), 2);  // emoji
  TM_CHECK_EQ(CharWidth(0x00A0), 1);   // no-break space
}
