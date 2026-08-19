// ANSI/VT parser state machine, spec §8.1 and §16.1: transitions including truncated
// and malformed sequences, and bounded work on hostile input.

#include <string>
#include <vector>

#include "framework.h"
#include "tm/term/parser.h"

using tmirror::ByteView;
using tmirror::term::Params;
using tmirror::term::Parser;
using tmirror::term::ParserHandler;

namespace {

/// Records everything the parser dispatches so a test can assert on the sequence.
class Recorder : public ParserHandler {
 public:
  std::string printed;
  std::vector<std::uint8_t> executed;
  std::vector<std::string> escapes;
  std::vector<std::string> csi;
  std::vector<std::string> osc;
  std::vector<std::string> dcs;
  int osc_truncations = 0;

  void OnPrint(char32_t code_point) override {
    if (code_point < 0x80) {
      printed.push_back(static_cast<char>(code_point));
    } else {
      printed += "<U+" + std::to_string(static_cast<unsigned>(code_point)) + ">";
    }
  }
  void OnExecute(std::uint8_t control) override { executed.push_back(control); }
  void OnEscape(const std::string& intermediates, std::uint8_t final_byte) override {
    escapes.push_back(intermediates + static_cast<char>(final_byte));
  }
  void OnCsi(const Params& params, std::uint8_t final_byte) override {
    std::string description;
    if (params.prefix() != 0) description.push_back(static_cast<char>(params.prefix()));
    for (int i = 0; i < params.count(); ++i) {
      if (i != 0) description.push_back(';');
      description += std::to_string(params.Get(i, -1));
      for (int sub = 1; sub < params.SubCount(i); ++sub) {
        description.push_back(':');
        description += std::to_string(params.GetSub(i, sub, -1));
      }
    }
    description += params.intermediates();
    description.push_back(static_cast<char>(final_byte));
    csi.push_back(description);
  }
  void OnOsc(const std::vector<std::string>& parts, bool truncated) override {
    std::string joined;
    for (std::size_t i = 0; i < parts.size(); ++i) {
      if (i != 0) joined.push_back('|');
      joined += parts[i];
    }
    osc.push_back(joined);
    if (truncated) ++osc_truncations;
  }
  void OnDcs(const Params& params, std::uint8_t final_byte, const std::string& data,
             bool truncated) override {
    (void)params;
    (void)truncated;
    dcs.push_back(std::string(1, static_cast<char>(final_byte)) + ":" + data);
  }
};

void FeedBytes(Parser& parser, const std::string& bytes, std::size_t chunk = 0) {
  if (chunk == 0) {
    parser.Feed(ByteView(bytes));
    return;
  }
  for (std::size_t offset = 0; offset < bytes.size(); offset += chunk) {
    std::size_t length = std::min(chunk, bytes.size() - offset);
    parser.Feed(ByteView::FromChars(bytes.data() + offset, length));
  }
}

}  // namespace

TM_TEST(Parser, ParsesPlainTextAndControls) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "hi\r\n\tthere\a");
  TM_CHECK_EQ(recorder.printed, "hithere");
  TM_REQUIRE(recorder.executed.size() == 4);
  TM_CHECK_EQ(static_cast<int>(recorder.executed[0]), 0x0D);
  TM_CHECK_EQ(static_cast<int>(recorder.executed[1]), 0x0A);
  TM_CHECK_EQ(static_cast<int>(recorder.executed[2]), 0x09);
  TM_CHECK_EQ(static_cast<int>(recorder.executed[3]), 0x07);
}

TM_TEST(Parser, ParsesCsiWithParametersAndPrefixes) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "\x1b[H\x1b[1;2H\x1b[?25l\x1b[38;5;196m\x1b[4:3m\x1b[ q");
  TM_REQUIRE(recorder.csi.size() == 6);
  TM_CHECK_EQ(recorder.csi[0], "H");
  TM_CHECK_EQ(recorder.csi[1], "1;2H");
  TM_CHECK_EQ(recorder.csi[2], "?25l");
  TM_CHECK_EQ(recorder.csi[3], "38;5;196m");
  TM_CHECK_EQ(recorder.csi[4], "4:3m");
  TM_CHECK_EQ(recorder.csi[5], " q");
}

TM_TEST(Parser, SplitsAcrossEveryChunkBoundary) {
  const std::string stream = "a\x1b[31mred\x1b]0;title\a\x1b[0mz";
  Recorder reference;
  Parser reference_parser(&reference);
  FeedBytes(reference_parser, stream);

  for (std::size_t chunk = 1; chunk <= stream.size(); ++chunk) {
    Recorder recorder;
    Parser parser(&recorder);
    FeedBytes(parser, stream, chunk);
    TM_CHECK_MSG(recorder.printed == reference.printed, "chunk " + std::to_string(chunk));
    TM_CHECK_MSG(recorder.csi == reference.csi, "chunk " + std::to_string(chunk));
    TM_CHECK_MSG(recorder.osc == reference.osc, "chunk " + std::to_string(chunk));
  }
}

TM_TEST(Parser, IgnoresUnknownAndMalformedSequences) {
  Recorder recorder;
  Parser parser(&recorder);
  // An unknown final byte, a private marker in an illegal position, and a stray
  // parameter byte after an intermediate all have to leave the parser usable.
  FeedBytes(parser, "\x1b[1;2\x01Z\x1b[1?2m\x1b[ !1mok");
  TM_CHECK_EQ(recorder.printed, "ok");
  TM_CHECK(parser.state() == Parser::State::kGround);
}

TM_TEST(Parser, EscapeRestartsAnyPendingSequence) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "\x1b[12;\x1b[H");
  TM_REQUIRE(recorder.csi.size() == 1);
  TM_CHECK_EQ(recorder.csi[0], "H");
}

TM_TEST(Parser, CancelAbortsSequence) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "\x1b[12\x18m");
  TM_CHECK(recorder.csi.empty());
  TM_CHECK_EQ(recorder.printed, "m");
}

TM_TEST(Parser, OscTerminatedByBelOrSt) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "\x1b]0;first\a\x1b]2;second\x1b\\");
  TM_REQUIRE(recorder.osc.size() == 2);
  TM_CHECK_EQ(recorder.osc[0], "0|first");
  TM_CHECK_EQ(recorder.osc[1], "2|second");
}

TM_TEST(Parser, OscIsBoundedButStillTerminates) {
  Recorder recorder;
  Parser parser(&recorder);
  Parser::Limits limits;
  limits.max_string_bytes = 64;
  parser.SetLimits(limits);

  std::string huge = "\x1b]0;" + std::string(100000, 'x') + "\a" + "after";
  FeedBytes(parser, huge);
  TM_REQUIRE(recorder.osc.size() == 1);
  TM_CHECK(recorder.osc[0].size() <= 70);
  TM_CHECK_EQ(recorder.osc_truncations, 1);
  // The parser must still be usable afterwards (spec §8.1).
  TM_CHECK_EQ(recorder.printed, "after");
}

TM_TEST(Parser, DcsIsConsumedAndBounded) {
  Recorder recorder;
  Parser parser(&recorder);
  Parser::Limits limits;
  limits.max_string_bytes = 32;
  parser.SetLimits(limits);
  FeedBytes(parser, "\x1bP1$r" + std::string(1000, 'q') + "\x1b\\done");
  TM_REQUIRE(recorder.dcs.size() == 1);
  TM_CHECK(recorder.dcs[0].size() <= 40);
  TM_CHECK_EQ(recorder.printed, "done");
}

TM_TEST(Parser, ApcAndPmStringsAreDiscarded) {
  Recorder recorder;
  Parser parser(&recorder);
  FeedBytes(parser, "\x1b_payload\x1b\\\x1b^private\x1b\\visible");
  TM_CHECK_EQ(recorder.printed, "visible");
  TM_CHECK(recorder.osc.empty());
}

TM_TEST(Parser, ParameterCountAndValueAreClamped) {
  Recorder recorder;
  Parser parser(&recorder);
  std::string many = "\x1b[";
  for (int i = 0; i < 100; ++i) many += "1;";
  many += "999999999m";
  FeedBytes(parser, many);
  TM_REQUIRE(recorder.csi.size() == 1);
  // 32 parameters at most, and no value above the clamp.
  TM_CHECK(recorder.csi[0].find("999999999") == std::string::npos);
  TM_CHECK(recorder.csi[0].size() < 200);
}

TM_TEST(Parser, ByteThatEndsAnIllFormedSequenceIsReexamined) {
  Recorder recorder;
  Parser parser(&recorder);
  // A truncated two-byte sequence followed by a carriage return: the CR must be
  // executed as a control, not printed as text, and not swallowed.
  FeedBytes(parser, std::string("\xC3") + "\r" + "ok");
  TM_CHECK_EQ(recorder.printed, "<U+65533>ok");
  TM_REQUIRE(recorder.executed.size() == 1);
  TM_CHECK_EQ(static_cast<int>(recorder.executed[0]), 0x0D);

  // The same, but the byte begins an escape sequence.
  Recorder second;
  Parser second_parser(&second);
  FeedBytes(second_parser, std::string("\xE2\x82") + "\x1b[31m" + "x");
  TM_CHECK_EQ(second.printed, "<U+65533>x");
  TM_REQUIRE(second.csi.size() == 1);
  TM_CHECK_EQ(second.csi[0], "31m");
}

TM_TEST(Parser, Utf8FlushesBeforeControlSequences) {
  Recorder recorder;
  Parser parser(&recorder);
  // A truncated multi-byte sequence directly followed by ESC: the partial sequence
  // becomes one replacement and the escape is parsed normally.
  FeedBytes(parser, std::string("\xE2\x82") + "\x1b[H");
  TM_CHECK_EQ(recorder.printed, "<U+65533>");
  TM_REQUIRE(recorder.csi.size() == 1);
  TM_CHECK_EQ(recorder.csi[0], "H");
}
