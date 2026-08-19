// Emulator modes, attributes and reports (spec §8.1).

#include <string>

#include "framework.h"
#include "helpers.h"
#include "tm/term/emulator.h"

using tmirror::ByteView;
using tmirror::term::Color;
using tmirror::term::CursorShape;
using tmirror::term::Emulator;
using tmirror::term::EmulatorConfig;
using tmirror::term::kFlagBold;
using tmirror::term::kFlagConceal;
using tmirror::term::kFlagCurlyUnderline;
using tmirror::term::kFlagFaint;
using tmirror::term::kFlagInverse;
using tmirror::term::kFlagItalic;
using tmirror::term::kFlagStrike;
using tmirror::term::kFlagUnderline;
using tmirror::term::MouseEncoding;
using tmirror::term::MouseTracking;
using tmtest::Feed;
using tmtest::RowText;
using tmtest::SmallConfig;

TM_TEST(Emulator, SgrTextAttributes) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b[1;3;4;7;9mx\x1b[0my");
  const tmirror::term::Cell& styled = emulator.active().line(0).at(0);
  TM_CHECK((styled.flags & kFlagBold) != 0);
  TM_CHECK((styled.flags & kFlagItalic) != 0);
  TM_CHECK((styled.flags & kFlagUnderline) != 0);
  TM_CHECK((styled.flags & kFlagInverse) != 0);
  TM_CHECK((styled.flags & kFlagStrike) != 0);
  TM_CHECK_EQ(static_cast<int>(emulator.active().line(0).at(1).flags), 0);
}

TM_TEST(Emulator, SgrResetsIndividualAttributes) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b[1;2m\x1b[22ma");
  const tmirror::term::Cell& cell = emulator.active().line(0).at(0);
  TM_CHECK((cell.flags & (kFlagBold | kFlagFaint)) == 0);
  Feed(emulator, "\x1b[8m\x1b[28mb");
  TM_CHECK((emulator.active().line(0).at(1).flags & kFlagConceal) == 0);
}

TM_TEST(Emulator, SgrColours) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b[31;42ma");
  TM_CHECK(emulator.active().line(0).at(0).fg == Color::Indexed(1));
  TM_CHECK(emulator.active().line(0).at(0).bg == Color::Indexed(2));

  Feed(emulator, "\x1b[91mb");
  TM_CHECK(emulator.active().line(0).at(1).fg == Color::Indexed(9));

  Feed(emulator, "\x1b[38;5;208mc");
  TM_CHECK(emulator.active().line(0).at(2).fg == Color::Indexed(208));

  Feed(emulator, "\x1b[38;2;10;20;30md");
  TM_CHECK(emulator.active().line(0).at(3).fg == Color::Rgb(10, 20, 30));

  Feed(emulator, "\x1b[39;49me");
  TM_CHECK(emulator.active().line(0).at(4).fg.is_default());
  TM_CHECK(emulator.active().line(0).at(4).bg.is_default());
}

TM_TEST(Emulator, SgrColonSubParameterForms) {
  Emulator emulator(SmallConfig(20, 2));
  // The colon form must not consume following parameters: `4:3` then `31` here.
  Feed(emulator, "\x1b[4:3;31ma");
  const tmirror::term::Cell& cell = emulator.active().line(0).at(0);
  TM_CHECK((cell.flags & kFlagCurlyUnderline) != 0);
  TM_CHECK(cell.fg == Color::Indexed(1));

  Feed(emulator, "\x1b[0m\x1b[38:2::1:2:3mb");
  TM_CHECK(emulator.active().line(0).at(1).fg == Color::Rgb(1, 2, 3));

  Feed(emulator, "\x1b[0m\x1b[38:5:9mc");
  TM_CHECK(emulator.active().line(0).at(2).fg == Color::Indexed(9));
}

TM_TEST(Emulator, MalformedSgrDoesNotCorruptFollowingParameters) {
  Emulator emulator(SmallConfig(20, 2));
  // 38 with a truncated specification: the parser must not swallow the 1m that
  // follows in a way that leaves the pen in a wrong state permanently.
  Feed(emulator, "\x1b[38m\x1b[1ma");
  TM_CHECK((emulator.active().line(0).at(0).flags & kFlagBold) != 0);
}

TM_TEST(Emulator, ModesAreReportedForTheInputEncoder) {
  Emulator emulator(SmallConfig(20, 2));
  TM_CHECK(!emulator.application_cursor_keys());
  Feed(emulator, "\x1b[?1h");
  TM_CHECK(emulator.application_cursor_keys());
  Feed(emulator, "\x1b=");
  TM_CHECK(emulator.application_keypad());
  Feed(emulator, "\x1b[?2004h");
  TM_CHECK(emulator.bracketed_paste());
  Feed(emulator, "\x1b[?1004h");
  TM_CHECK(emulator.focus_reporting());
  Feed(emulator, "\x1b[?25l");
  TM_CHECK(!emulator.cursor_visible());
  Feed(emulator, "\x1b[20h");
  TM_CHECK(emulator.newline_mode());
}

TM_TEST(Emulator, MouseModesAndEncodings) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b[?1000h");
  TM_CHECK(emulator.mouse_tracking() == MouseTracking::kNormal);
  Feed(emulator, "\x1b[?1002h");
  TM_CHECK(emulator.mouse_tracking() == MouseTracking::kButtonEvent);
  Feed(emulator, "\x1b[?1006h");
  TM_CHECK(emulator.mouse_encoding() == MouseEncoding::kSgr);
  Feed(emulator, "\x1b[?1000l");
  TM_CHECK(emulator.mouse_tracking() == MouseTracking::kOff);
}

TM_TEST(Emulator, CursorStyleSequence) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b[4 q");
  TM_CHECK(emulator.cursor_shape() == CursorShape::kUnderline);
  TM_CHECK(!emulator.cursor_blinking());
  Feed(emulator, "\x1b[5 q");
  TM_CHECK(emulator.cursor_shape() == CursorShape::kBar);
  TM_CHECK(emulator.cursor_blinking());
}

TM_TEST(Emulator, WindowTitleIsSanitisedAndReported) {
  Emulator emulator(SmallConfig(20, 2));
  std::string reported;
  emulator.SetTitleCallback([&](const std::string& title) { reported = title; });
  Feed(emulator, "\x1b]0;build \x07shell\x1b\\");
  // The BEL terminates the OSC, so the title is what preceded it.
  TM_CHECK_EQ(emulator.title(), "build ");
  TM_CHECK_EQ(reported, "build ");

  Feed(emulator, "\x1b]2;plain title\x1b\\");
  TM_CHECK_EQ(emulator.title(), "plain title");
}

TM_TEST(Emulator, TitleLengthIsBounded) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "\x1b]2;" + std::string(5000, 'x') + "\x07");
  TM_CHECK(emulator.title().size() <= 300);
}

TM_TEST(Emulator, ClipboardReadIsNeverAnswered) {
  EmulatorConfig config = SmallConfig(20, 2);
  config.allow_clipboard_write = true;
  Emulator emulator(config);
  int writes = 0;
  emulator.SetClipboardCallback([&](const std::string&) { ++writes; });
  Feed(emulator, "\x1b]52;c;?\x07");
  TM_CHECK_EQ(writes, 0);
  Feed(emulator, "\x1b]52;c;aGVsbG8=\x07");  // "hello"
  TM_CHECK_EQ(writes, 1);
}

TM_TEST(Emulator, ClipboardWriteIsRefusedByDefault) {
  Emulator emulator(SmallConfig(20, 2));
  int writes = 0;
  emulator.SetClipboardCallback([&](const std::string&) { ++writes; });
  Feed(emulator, "\x1b]52;c;aGVsbG8=\x07");
  TM_CHECK_EQ(writes, 0);
}

TM_TEST(Emulator, DeviceAttributesAndStatusReports) {
  Emulator emulator(SmallConfig(20, 4));
  std::string responses;
  emulator.SetResponseSink([&](ByteView bytes) { responses += bytes.to_string(); });

  Feed(emulator, "\x1b[c");
  TM_CHECK_EQ(responses, "\x1b[?62;22c");

  responses.clear();
  Feed(emulator, "\x1b[2;3H\x1b[6n");
  TM_CHECK_EQ(responses, "\x1b[2;3R");

  responses.clear();
  Feed(emulator, "\x1b[5n");
  TM_CHECK_EQ(responses, "\x1b[0n");

  responses.clear();
  Feed(emulator, "\x1b[>c");
  TM_CHECK_EQ(responses, "\x1b[>0;276;0c");
}

TM_TEST(Emulator, DeviceQueriesCanBeDisabled) {
  EmulatorConfig config = SmallConfig(20, 2);
  config.answer_device_queries = false;
  Emulator emulator(config);
  std::string responses;
  emulator.SetResponseSink([&](ByteView bytes) { responses += bytes.to_string(); });
  Feed(emulator, "\x1b[c\x1b[6n");
  TM_CHECK_EQ(responses, "");
}

TM_TEST(Emulator, ModeQueryReportsState) {
  Emulator emulator(SmallConfig(20, 2));
  std::string responses;
  emulator.SetResponseSink([&](ByteView bytes) { responses += bytes.to_string(); });
  Feed(emulator, "\x1b[?25$p");
  TM_CHECK_EQ(responses, "\x1b[?25;1$y");
  responses.clear();
  Feed(emulator, "\x1b[?25l\x1b[?25$p");
  TM_CHECK_EQ(responses, "\x1b[?25;2$y");
}

TM_TEST(Emulator, ResetRestoresEverything) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "\x1b[?1h\x1b[?1049h\x1b[31mtext\x1b]2;title\x07");
  emulator.Reset();
  TM_CHECK(!emulator.application_cursor_keys());
  TM_CHECK(!emulator.alt_screen_active());
  TM_CHECK_EQ(emulator.title(), "");
  TM_CHECK_EQ(RowText(emulator, 0), "");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(0));
}

TM_TEST(Emulator, RisSequencePerformsAFullReset) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "\x1b[31mtext\x1b" "c");
  TM_CHECK_EQ(RowText(emulator, 0), "");
  Feed(emulator, "x");
  TM_CHECK(emulator.active().line(0).at(0).fg.is_default());
}

TM_TEST(Emulator, SoftResetKeepsTheScreen) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "hello\x1b[?6h\x1b[!p");
  TM_CHECK_EQ(RowText(emulator, 0), "hello");
  TM_CHECK(!emulator.active().origin_mode());
}

TM_TEST(Emulator, RevisionAdvancesOnlyOnChange) {
  Emulator emulator(SmallConfig(10, 3));
  std::uint64_t before = emulator.revision();
  Feed(emulator, "");
  TM_CHECK_EQ(emulator.revision(), before);
  Feed(emulator, "x");
  TM_CHECK(emulator.revision() > before);
}

TM_TEST(Emulator, SnapshotFollowsScrollOffset) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "1\r\n2\r\n3\r\n4");
  tmirror::term::Snapshot live = emulator.BuildSnapshot(0);
  TM_CHECK_EQ(live.scroll_offset, static_cast<std::size_t>(0));
  TM_CHECK_EQ(live.cursor.row, 1);

  tmirror::term::Snapshot scrolled = emulator.BuildSnapshot(2);
  TM_CHECK_EQ(scrolled.scroll_offset, static_cast<std::size_t>(2));
  // Scrolled two lines back, the cursor is no longer in view.
  TM_CHECK_EQ(scrolled.cursor.row, -1);
  TM_REQUIRE(scrolled.line(0) != nullptr);
  TM_CHECK_EQ(scrolled.line(0)->at(0).code, U'1');
}

TM_TEST(Emulator, SnapshotClampsAnExcessiveScrollOffset) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "1\r\n2\r\n3");
  tmirror::term::Snapshot snapshot = emulator.BuildSnapshot(1000);
  TM_CHECK_EQ(snapshot.scroll_offset, emulator.scrollback().size());
}

TM_TEST(Emulator, SnapshotSharesLinesRatherThanCopying) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "shared");
  tmirror::term::Snapshot first = emulator.BuildSnapshot(0);
  const tmirror::term::Line* line = first.line(0);
  TM_REQUIRE(line != nullptr);
  // Writing again must not mutate the snapshot the renderer already holds.
  Feed(emulator, "\rmodified");
  TM_CHECK_EQ(line->at(0).code, U's');
  TM_CHECK_EQ(emulator.active().line(0).at(0).code, U'm');
}
