// Screen operations, spec §8.2 and §16.1: attributes, wide/combining characters,
// scroll regions, the alternate screen, and the resize policy.

#include "framework.h"
#include "helpers.h"
#include "tm/term/emulator.h"

using tmirror::term::Cell;
using tmirror::term::Emulator;
using tmirror::term::EmulatorConfig;
using tmirror::term::kFlagBold;
using tmirror::term::kFlagHasMarks;
using tmtest::Feed;
using tmtest::RowText;
using tmtest::ScreenText;
using tmtest::SmallConfig;

TM_TEST(Screen, WritesTextAndTracksCursor) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "hello");
  TM_CHECK_EQ(RowText(emulator, 0), "hello");
  TM_CHECK_EQ(emulator.active().cursor_column(), 5);
  TM_CHECK_EQ(emulator.active().cursor_row(), 0);
}

TM_TEST(Screen, DeferredWrapHappensOnTheNextCharacter) {
  Emulator emulator(SmallConfig(5, 3));
  Feed(emulator, "abcde");
  // The cursor sits on the last column with a pending wrap, not on the next line:
  // this is what makes `echo -n 12345; echo -n X` land correctly.
  TM_CHECK_EQ(emulator.active().cursor_row(), 0);
  TM_CHECK_EQ(emulator.active().cursor_column(), 4);
  TM_CHECK(emulator.active().pending_wrap());
  Feed(emulator, "f");
  TM_CHECK_EQ(emulator.active().cursor_row(), 1);
  TM_CHECK_EQ(RowText(emulator, 0), "abcde");
  TM_CHECK_EQ(RowText(emulator, 1), "f");
  TM_CHECK(emulator.active().line(0).wrapped());
}

TM_TEST(Screen, AutowrapOffOverwritesTheLastColumn) {
  Emulator emulator(SmallConfig(5, 3));
  Feed(emulator, "\x1b[?7l");
  Feed(emulator, "abcdefg");
  TM_CHECK_EQ(RowText(emulator, 0), "abcdg");
  TM_CHECK_EQ(RowText(emulator, 1), "");
}

TM_TEST(Screen, WideCharacterOccupiesTwoCells) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "a\xE4\xB8\x80" "b");  // a 一 b
  const tmirror::term::Line& line = emulator.active().line(0);
  TM_CHECK_EQ(static_cast<int>(line.at(1).width), 2);
  TM_CHECK(line.at(2).is_continuation());
  TM_CHECK_EQ(line.at(3).code, U'b');
  TM_CHECK_EQ(emulator.active().cursor_column(), 4);
}

TM_TEST(Screen, WideCharacterNeverStraddlesTheMargin) {
  Emulator emulator(SmallConfig(4, 3));
  Feed(emulator, "abc\xE4\xB8\x80");
  // Only three columns are used before the wide character, so it wraps whole.
  TM_CHECK_EQ(RowText(emulator, 0), "abc");
  TM_CHECK_EQ(emulator.active().line(1).at(0).width, 2);
}

TM_TEST(Screen, OverwritingHalfOfAWideCharacterClearsBoth) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "\xE4\xB8\x80");           // wide char at columns 0-1
  Feed(emulator, "\x1b[1;2Hx");             // write over the continuation cell
  const tmirror::term::Line& line = emulator.active().line(0);
  TM_CHECK_EQ(line.at(0).code, U' ');
  TM_CHECK_EQ(line.at(1).code, U'x');
}

TM_TEST(Screen, CombiningMarksAttachToThePrecedingCell) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "e\xCC\x81");  // e + combining acute
  const tmirror::term::Line& line = emulator.active().line(0);
  TM_CHECK_EQ(line.at(0).code, U'e');
  TM_CHECK((line.at(0).flags & kFlagHasMarks) != 0);
  const std::u32string* marks = line.Marks(0);
  TM_REQUIRE(marks != nullptr);
  TM_CHECK_EQ(marks->size(), static_cast<std::size_t>(1));
  TM_CHECK_EQ((*marks)[0], static_cast<char32_t>(0x0301));
  TM_CHECK_EQ(emulator.active().cursor_column(), 1);
}

TM_TEST(Screen, CombiningMarksAreBounded) {
  Emulator emulator(SmallConfig(6, 2));
  std::string input = "e";
  for (int i = 0; i < 100; ++i) input += "\xCC\x81";
  Feed(emulator, input);
  const std::u32string* marks = emulator.active().line(0).Marks(0);
  TM_REQUIRE(marks != nullptr);
  TM_CHECK(marks->size() <= tmirror::term::Line::kMaxMarksPerCell);
}

TM_TEST(Screen, EraseInLineRespectsTheBackgroundColour) {
  Emulator emulator(SmallConfig(8, 2));
  Feed(emulator, "abcdefgh\x1b[1;3H\x1b[41m\x1b[K");
  const tmirror::term::Line& line = emulator.active().line(0);
  TM_CHECK_EQ(RowText(emulator, 0), "ab");
  TM_CHECK(line.at(4).bg == tmirror::term::Color::Indexed(1));
  TM_CHECK(line.at(1).bg.is_default());
}

TM_TEST(Screen, EraseInDisplayModes) {
  Emulator emulator(SmallConfig(4, 3));
  Feed(emulator, "aaaa\r\nbbbb\r\ncccc");
  Feed(emulator, "\x1b[2;2H\x1b[J");  // erase below, from the cursor
  TM_CHECK_EQ(RowText(emulator, 0), "aaaa");
  TM_CHECK_EQ(RowText(emulator, 1), "b");
  TM_CHECK_EQ(RowText(emulator, 2), "");

  Feed(emulator, "\x1b[2J");
  TM_CHECK_EQ(ScreenText(emulator), "");
}

TM_TEST(Screen, InsertAndDeleteCharacters) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "abcdef\x1b[1;3H\x1b[2@");
  TM_CHECK_EQ(RowText(emulator, 0), "ab  cd");
  Feed(emulator, "\x1b[1;3H\x1b[2P");
  TM_CHECK_EQ(RowText(emulator, 0), "abcd");
}

TM_TEST(Screen, InsertAndDeleteLinesWithinTheScrollRegion) {
  Emulator emulator(SmallConfig(4, 5));
  Feed(emulator, "1\r\n2\r\n3\r\n4\r\n5");
  Feed(emulator, "\x1b[2;4r");   // region rows 2..4
  Feed(emulator, "\x1b[2;1H\x1b[L");
  TM_CHECK_EQ(RowText(emulator, 0), "1");
  TM_CHECK_EQ(RowText(emulator, 1), "");
  TM_CHECK_EQ(RowText(emulator, 2), "2");
  TM_CHECK_EQ(RowText(emulator, 3), "3");
  TM_CHECK_EQ(RowText(emulator, 4), "5");  // outside the region, untouched
}

TM_TEST(Screen, ScrollRegionScrollsOnlyItself) {
  Emulator emulator(SmallConfig(4, 4));
  Feed(emulator, "1\r\n2\r\n3\r\n4");
  Feed(emulator, "\x1b[2;3r\x1b[3;1H\r\n");  // LF at the region bottom scrolls it
  TM_CHECK_EQ(RowText(emulator, 0), "1");
  TM_CHECK_EQ(RowText(emulator, 1), "3");
  TM_CHECK_EQ(RowText(emulator, 2), "");
  TM_CHECK_EQ(RowText(emulator, 3), "4");
}

TM_TEST(Screen, ScrollingPushesLinesToScrollback) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "1\r\n2\r\n3\r\n4");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(2));
  TM_CHECK_EQ(RowText(emulator, 0), "3");
  TM_CHECK_EQ(RowText(emulator, 1), "4");
}

TM_TEST(Screen, DeleteLineDoesNotFillScrollback) {
  Emulator emulator(SmallConfig(4, 3));
  Feed(emulator, "1\r\n2\r\n3");
  std::size_t before = emulator.scrollback().size();
  Feed(emulator, "\x1b[1;1H\x1b[M");
  TM_CHECK_EQ(emulator.scrollback().size(), before);
}

TM_TEST(Screen, TabStopsDefaultAndCustom) {
  Emulator emulator(SmallConfig(20, 2));
  Feed(emulator, "a\tb");
  TM_CHECK_EQ(emulator.active().cursor_column(), 9);
  Feed(emulator, "\r\x1b[3g");  // clear all tab stops
  Feed(emulator, "\t");
  // With no stops left, HT runs to the last column rather than wrapping.
  TM_CHECK_EQ(emulator.active().cursor_column(), 19);
}

TM_TEST(Screen, SaveAndRestoreCursorIncludesAttributes) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "\x1b[2;3H\x1b[1m\x1b" "7");
  Feed(emulator, "\x1b[1;1H\x1b[0mx\x1b" "8y");
  TM_CHECK_EQ(emulator.active().cursor_row(), 1);
  TM_CHECK_EQ(RowText(emulator, 1), "  y");
  TM_CHECK((emulator.active().line(1).at(2).flags & kFlagBold) != 0);
}

TM_TEST(Screen, OriginModeMakesAddressingRelative) {
  Emulator emulator(SmallConfig(6, 5));
  Feed(emulator, "\x1b[2;4r\x1b[?6h\x1b[1;1Hx");
  TM_CHECK_EQ(RowText(emulator, 1), "x");
  Feed(emulator, "\x1b[?6l\x1b[1;1Hy");
  TM_CHECK_EQ(RowText(emulator, 0), "y");
}

TM_TEST(Screen, AlternateScreenKeepsThePrimaryIntact) {
  Emulator emulator(SmallConfig(6, 3));
  Feed(emulator, "primary");
  Feed(emulator, "\x1b[?1049h");
  TM_CHECK(emulator.alt_screen_active());
  TM_CHECK_EQ(ScreenText(emulator), "");
  Feed(emulator, "alt");
  TM_CHECK_EQ(RowText(emulator, 0), "alt");
  Feed(emulator, "\x1b[?1049l");
  TM_CHECK(!emulator.alt_screen_active());
  TM_CHECK_EQ(RowText(emulator, 0), "primar");
}

TM_TEST(Screen, AlternateScreenDoesNotFillScrollback) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "\x1b[?1049h");
  Feed(emulator, "1\r\n2\r\n3\r\n4\r\n5");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(0));
}

TM_TEST(Screen, ReverseIndexScrollsDownAtTheTop) {
  Emulator emulator(SmallConfig(4, 3));
  Feed(emulator, "1\r\n2\r\n3");
  Feed(emulator, "\x1b[1;1H\x1bM");
  TM_CHECK_EQ(RowText(emulator, 0), "");
  TM_CHECK_EQ(RowText(emulator, 1), "1");
}

TM_TEST(Screen, RepeatRepeatsTheLastGlyph) {
  Emulator emulator(SmallConfig(10, 2));
  Feed(emulator, "a\x1b[4b");
  TM_CHECK_EQ(RowText(emulator, 0), "aaaaa");
}

TM_TEST(Screen, DecAlignmentTestFillsTheScreen) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "\x1b#8");
  TM_CHECK_EQ(RowText(emulator, 0), "EEEE");
  TM_CHECK_EQ(RowText(emulator, 1), "EEEE");
}

TM_TEST(Screen, DecSpecialGraphicsDrawsBoxCharacters) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "\x1b(0lqk\x1b(B");
  const tmirror::term::Line& line = emulator.active().line(0);
  TM_CHECK_EQ(line.at(0).code, static_cast<char32_t>(0x250C));
  TM_CHECK_EQ(line.at(1).code, static_cast<char32_t>(0x2500));
  TM_CHECK_EQ(line.at(2).code, static_cast<char32_t>(0x2510));
}

TM_TEST(Screen, ShiftOutSelectsG1) {
  Emulator emulator(SmallConfig(6, 2));
  Feed(emulator, "\x1b)0\x0Eq\x0Fq");
  TM_CHECK_EQ(emulator.active().line(0).at(0).code, static_cast<char32_t>(0x2500));
  TM_CHECK_EQ(emulator.active().line(0).at(1).code, U'q');
}

TM_TEST(Screen, ResizeNarrowerReflowsWrappedText) {
  EmulatorConfig config = SmallConfig(10, 3);
  Emulator emulator(config);
  Feed(emulator, "abcdefghijklm");  // wraps at 10 columns
  TM_CHECK_EQ(RowText(emulator, 0), "abcdefghij");
  TM_CHECK_EQ(RowText(emulator, 1), "klm");

  emulator.Resize(5, 3);
  TM_CHECK_EQ(emulator.columns(), 5);
  // The same logical line, rewrapped: nothing is lost.
  std::string joined;
  for (std::size_t i = 0; i < emulator.scrollback().size(); ++i) {
    const tmirror::term::Line& line = *emulator.scrollback().at(i);
    for (std::size_t c = 0; c < line.size(); ++c) {
      if (line.at(c).code != 0 && line.at(c).code != U' ') {
        tmirror::term::AppendUtf8(line.at(c).code, &joined);
      }
    }
  }
  for (int row = 0; row < emulator.rows(); ++row) joined += RowText(emulator, row);
  TM_CHECK_EQ(joined, "abcdefghijklm");
}

TM_TEST(Screen, ResizeWiderRejoinsWrappedText) {
  Emulator emulator(SmallConfig(5, 4));
  Feed(emulator, "abcdefghij");
  emulator.Resize(10, 4);
  TM_CHECK_EQ(RowText(emulator, 0), "abcdefghij");
}

TM_TEST(Screen, ResizeKeepsTheCursorOnItsCharacter) {
  Emulator emulator(SmallConfig(10, 4));
  Feed(emulator, "abcdefghijklmno");
  int before_column = emulator.active().cursor_column();
  TM_CHECK_EQ(before_column, 5);
  emulator.Resize(5, 4);
  // Fifteen characters at width five fill three rows exactly. The cursor sat one
  // past the last character, and a position past the right margin is clamped to the
  // last column by the documented resize policy (docs/resize-policy.md).
  TM_CHECK_EQ(emulator.active().cursor_column(), 4);
  TM_CHECK_EQ(RowText(emulator, emulator.active().cursor_row()), "klmno");
}

TM_TEST(Screen, GrowingRowsPullsContentBackFromScrollback) {
  Emulator emulator(SmallConfig(20, 6));
  for (int i = 1; i <= 6; ++i) Feed(emulator, "line " + std::to_string(i) + "\r\n");
  // Six lines and six newlines on a six-row screen: one line has scrolled off, and
  // the cursor sits on the blank last row.
  TM_CHECK_EQ(RowText(emulator, 0), "line 2");
  TM_CHECK_EQ(RowText(emulator, 4), "line 6");
  TM_CHECK_EQ(emulator.active().cursor_row(), 5);

  // A soft keyboard opens: the grid shrinks and rows move into the scrollback.
  emulator.Resize(20, 2);
  TM_CHECK_EQ(RowText(emulator, 0), "line 6");
  std::size_t scrolled_away = emulator.scrollback().size();
  TM_CHECK(scrolled_away >= 5);

  // It closes again. The rows that left come back, so the screen is exactly what it
  // was before the shrink rather than four blank rows and one line of text.
  emulator.Resize(20, 6);
  TM_CHECK_EQ(RowText(emulator, 0), "line 2");
  TM_CHECK_EQ(RowText(emulator, 4), "line 6");
  TM_CHECK_EQ(emulator.active().cursor_row(), 5);
  TM_CHECK(emulator.scrollback().size() < scrolled_away);
}

TM_TEST(Screen, GrowingRowsWithNoScrollbackAddsBlankLines) {
  Emulator emulator(SmallConfig(20, 3));
  Feed(emulator, "only line");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(0));
  emulator.Resize(20, 8);
  TM_CHECK_EQ(emulator.rows(), 8);
  TM_CHECK_EQ(RowText(emulator, 0), "only line");
  TM_CHECK_EQ(emulator.active().cursor_row(), 0);
}

TM_TEST(Screen, AlternateScreenResizeDoesNotReflow) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "\x1b[?1049h");
  Feed(emulator, "abcdefghij");
  emulator.Resize(5, 3);
  // Grid semantics: the row is clipped, not rewrapped (spec §8.2).
  TM_CHECK_EQ(RowText(emulator, 0), "abcde");
  TM_CHECK_EQ(RowText(emulator, 1), "");
}

TM_TEST(Screen, ScrollbackIsBoundedByLinesAndBytes) {
  EmulatorConfig config = SmallConfig(20, 2);
  config.scrollback.max_lines = 5;
  Emulator emulator(config);
  for (int i = 0; i < 50; ++i) Feed(emulator, "line\r\n");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(5));

  EmulatorConfig tiny = SmallConfig(80, 2);
  tiny.scrollback.max_lines = 10000;
  tiny.scrollback.max_bytes = 4096;
  Emulator bounded(tiny);
  for (int i = 0; i < 2000; ++i) Feed(bounded, "some text to store\r\n");
  TM_CHECK(bounded.scrollback().memory_bytes() <= 4096 + 2048);
  TM_CHECK(bounded.scrollback().size() < 10000);
}

TM_TEST(Screen, EraseInDisplayThreeClearsScrollback) {
  Emulator emulator(SmallConfig(4, 2));
  Feed(emulator, "1\r\n2\r\n3\r\n4");
  TM_CHECK(emulator.scrollback().size() > 0);
  Feed(emulator, "\x1b[3J");
  TM_CHECK_EQ(emulator.scrollback().size(), static_cast<std::size_t>(0));
}
