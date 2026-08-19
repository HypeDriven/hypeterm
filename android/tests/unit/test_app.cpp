// Session view state, selection extraction and preference persistence
// (spec §5.2, §6.1, §13).

#include <cstdio>
#include <string>

#include "framework.h"
#include "helpers.h"
#include "tm/app/config.h"
#include "tm/app/persistence.h"
#include "tm/app/session.h"

using tmirror::app::AppConfig;
using tmirror::app::ExtractSelection;
using tmirror::app::ExtractVisibleText;
using tmirror::app::Preferences;
using tmirror::app::TerminalSession;
using tmirror::term::Selection;
using tmirror::term::Snapshot;

namespace {

AppConfig TestConfig() {
  AppConfig config;
  config.server_url = "https://relay.example";
  config.fallback_columns = 10;
  config.fallback_rows = 4;
  config.scrollback.max_lines = 50;
  return config;
}

void FeedText(TerminalSession* session, const std::string& text) {
  session->ApplyOutput(tmirror::ByteView(text));
}

std::string TempPath(const char* name) {
  const char* base = std::getenv("TMPDIR");
  std::string directory = base != nullptr ? base : "/tmp";
  return directory + "/tm_test_" + name;
}

}  // namespace

TM_TEST(App, ConfigValidationRejectsNonsense) {
  AppConfig config = TestConfig();
  TM_CHECK(config.Validate().ok());

  config.server_url = "not a url";
  TM_CHECK(!config.Validate().ok());

  config = TestConfig();
  config.fallback_columns = 0;
  TM_CHECK(!config.Validate().ok());

  config = TestConfig();
  config.paste_max_bytes = 1;
  config.paste_chunk_bytes = 4096;
  TM_CHECK(!config.Validate().ok());
}

TM_TEST(App, SessionPublishesSnapshotsOnlyWhenChanged) {
  TerminalSession session(TestConfig());
  TM_CHECK(session.NeedsPublish());
  session.PublishSnapshot();
  TM_CHECK(!session.NeedsPublish());

  FeedText(&session, "hello");
  TM_CHECK(session.NeedsPublish());
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  TM_CHECK_EQ(session.latest_snapshot(), snapshot);
  TM_CHECK(!session.NeedsPublish());
}

TM_TEST(App, ScrollOffsetIsClampedToRetainedScrollback) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 20; ++i) FeedText(&session, "line\r\n");
  session.ScrollLines(1000);
  TM_CHECK_EQ(session.scroll_offset(), session.emulator().max_scroll_offset());
  session.ScrollLines(-1000);
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(0));
  TM_CHECK(session.following_output());
}

TM_TEST(App, ScrolledBackViewportFollowsItsContent) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 20; ++i) FeedText(&session, "line " + std::to_string(i) + "\r\n");

  // Scroll back three lines and remember what is on the top visible row.
  session.ScrollLines(3);
  tmirror::term::SnapshotRef before = session.PublishSnapshot();
  TM_REQUIRE(before != nullptr);
  TM_REQUIRE(before->line(0) != nullptr);
  const char32_t first_code = before->line(0)->at(0).code;
  const char32_t sixth_code = before->line(0)->at(5).code;

  // More output arrives. The viewport must keep showing the same content rather than
  // drifting as lines move into the scrollback.
  for (int i = 0; i < 5; ++i) FeedText(&session, "new line\r\n");
  tmirror::term::SnapshotRef after = session.PublishSnapshot();
  TM_REQUIRE(after != nullptr);
  TM_REQUIRE(after->line(0) != nullptr);
  TM_CHECK_EQ(after->line(0)->at(0).code, first_code);
  TM_CHECK_EQ(after->line(0)->at(5).code, sixth_code);
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(8));

  // Returning to the bottom follows live output again.
  session.ScrollToBottom();
  TM_CHECK(session.following_output());
}

TM_TEST(App, AScrolledBackViewportHoldsStillEvenWhenTheScrollbackIsFull) {
  AppConfig config = TestConfig();
  config.scrollback.max_lines = 8;
  TerminalSession session(config);

  // Fill the ring past its limit, then park the reader inside it.
  for (int i = 0; i < 20; ++i) FeedText(&session, "old\r\n");
  TM_REQUIRE(session.emulator().max_scroll_offset() == 8);
  session.ScrollLines(4);

  // From here the ring evicts exactly as fast as it fills, so its *size* never changes
  // again while the live bottom keeps moving. Measuring the size would report no
  // movement at the moment there is most of it, and the view would slide one line per
  // line of output — during a long build, which is precisely when somebody is scrolled
  // back reading an error.
  for (int i = 0; i < 2; ++i) FeedText(&session, "new\r\n");
  TM_CHECK_EQ(session.emulator().max_scroll_offset(), static_cast<std::size_t>(8));
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(6));

  // Once more output has gone past than the ring can hold, the offset stops at the
  // oldest line still retained rather than running off the end of it.
  for (int i = 0; i < 20; ++i) FeedText(&session, "new\r\n");
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(8));
  TM_CHECK(!session.following_output());
}

TM_TEST(App, ThePublishedSnapshotSaysWhetherTheSessionIsAtTheLatestOutput) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 12; ++i) FeedText(&session, "line\r\n");

  TM_CHECK(session.PublishSnapshot()->following_output);
  session.ScrollLines(4);
  TM_CHECK(!session.PublishSnapshot()->following_output);
  session.ScrollToBottom();
  TM_CHECK(session.PublishSnapshot()->following_output);
}

TM_TEST(App, TheAlternateScreenDoesNotLookLikeAReturnToTheLatestOutput) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 12; ++i) FeedText(&session, "line\r\n");
  session.ScrollLines(4);

  // The alternate screen keeps no scrollback (spec §8.2), so the emulator clamps the
  // offset it is handed to zero there. Reading the clamped value would report somebody
  // who ran `less` from a scrolled-back prompt as having gone back to the bottom, and
  // the screen would take away their way back.
  FeedText(&session, "\x1b[?1049h");
  tmirror::term::SnapshotRef alt = session.PublishSnapshot();
  TM_CHECK_EQ(alt->scroll_offset, static_cast<std::size_t>(0));
  TM_CHECK(!alt->following_output);

  FeedText(&session, "\x1b[?1049l");
  TM_CHECK(!session.PublishSnapshot()->following_output);
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(4));
}

TM_TEST(App, ADragInsideAFullScreenApplicationDoesNotParkTheSessionInHistory) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 12; ++i) FeedText(&session, "line\r\n");
  FeedText(&session, "\x1b[?1049h");

  // A full-screen application that does not track the mouse still receives drag
  // gestures as local scrollback movement. There is no scrollback to move through, so
  // the screen would not budge — but the offset would count up against the primary
  // screen's history, quietly leaving the session somewhere other than the live bottom.
  session.ScrollLines(5);
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(0));
  TM_CHECK(session.PublishSnapshot()->following_output);

  // Leaving it, the primary screen's own position is untouched and still scrollable.
  FeedText(&session, "\x1b[?1049l");
  session.ScrollLines(5);
  TM_CHECK_EQ(session.scroll_offset(), static_cast<std::size_t>(5));
}

TM_TEST(App, ResizeClearsSelectionAndClampsScroll) {
  TerminalSession session(TestConfig());
  for (int i = 0; i < 10; ++i) FeedText(&session, "line\r\n");
  Selection selection;
  selection.active = true;
  session.SetSelection(selection);
  session.ScrollLines(3);
  session.ResizeGrid(20, 8);
  TM_CHECK(!session.selection().active);
  TM_CHECK(session.scroll_offset() <= session.emulator().max_scroll_offset());
}

TM_TEST(App, SelectionExtractionJoinsWrappedLines) {
  TerminalSession session(TestConfig());
  FeedText(&session, "abcdefghijklmno");  // wraps at 10 columns

  Selection selection;
  selection.active = true;
  selection.start_row = 0;
  selection.start_column = 0;
  selection.end_row = 1;
  selection.end_column = 4;
  session.SetSelection(selection);
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  // A wrapped line is one logical line, so no newline is inserted at the wrap.
  TM_CHECK_EQ(ExtractSelection(*snapshot), "abcdefghijklmno");
}

TM_TEST(App, SelectionExtractionKeepsRealNewlines) {
  TerminalSession session(TestConfig());
  FeedText(&session, "one\r\ntwo");
  Selection selection;
  selection.active = true;
  selection.start_row = 0;
  selection.start_column = 0;
  selection.end_row = 1;
  selection.end_column = 9;
  session.SetSelection(selection);
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  TM_CHECK_EQ(ExtractSelection(*snapshot), "one\ntwo");
}

TM_TEST(App, RectangularSelectionTakesAColumnRange) {
  TerminalSession session(TestConfig());
  FeedText(&session, "abcdef\r\nghijkl");
  Selection selection;
  selection.active = true;
  selection.rectangular = true;
  selection.start_row = 0;
  selection.start_column = 1;
  selection.end_row = 1;
  selection.end_column = 2;
  session.SetSelection(selection);
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  TM_CHECK_EQ(ExtractSelection(*snapshot), "bc\nhi");
}

TM_TEST(App, SelectionIncludesCombiningMarksAndSkipsContinuations) {
  TerminalSession session(TestConfig());
  FeedText(&session, "e\xCC\x81\xE4\xB8\x80x");
  Selection selection;
  selection.active = true;
  selection.start_row = 0;
  selection.start_column = 0;
  selection.end_row = 0;
  selection.end_column = 9;
  session.SetSelection(selection);
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  TM_CHECK_EQ(ExtractSelection(*snapshot), "e\xCC\x81\xE4\xB8\x80x");
}

TM_TEST(App, VisibleTextIsWhatTheAccessibilityBridgeReads) {
  TerminalSession session(TestConfig());
  FeedText(&session, "line one\r\nline two");
  tmirror::term::SnapshotRef snapshot = session.PublishSnapshot();
  TM_REQUIRE(snapshot != nullptr);
  std::string text = ExtractVisibleText(*snapshot);
  TM_CHECK(text.find("line one") != std::string::npos);
  TM_CHECK(text.find("line two") != std::string::npos);
}

TM_TEST(App, KeyboardModesComeFromTheEmulator) {
  TerminalSession session(TestConfig());
  FeedText(&session, "\x1b[?1h\x1b[?2004h\x1b=");
  tmirror::input::KeyboardModes modes = session.keyboard_modes();
  TM_CHECK(modes.application_cursor);
  TM_CHECK(modes.bracketed_paste);
  TM_CHECK(modes.application_keypad);
}

TM_TEST(Preferences, RoundTripsThroughAFile) {
  std::string path = TempPath("prefs.json");
  std::remove(path.c_str());

  {
    Preferences preferences(path);
    TM_CHECK(preferences.Load().ok());  // a missing file is not an error
    preferences.SetString("server_url", "https://relay.example");
    preferences.SetInt("font_size", 15);
    preferences.SetBool("secure_window", true);
    preferences.SetResumeOffset("terminal-a", 4096, 1700000000000LL);
    TM_CHECK(preferences.Save().ok());
  }
  {
    Preferences preferences(path);
    TM_CHECK(preferences.Load().ok());
    TM_CHECK_EQ(preferences.GetString("server_url"), "https://relay.example");
    TM_CHECK_EQ(preferences.GetInt("font_size", 0), 15LL);
    TM_CHECK(preferences.GetBool("secure_window", false));
    std::uint64_t offset = 0;
    TM_CHECK(preferences.GetResumeOffset("terminal-a", &offset));
    TM_CHECK_EQ(offset, static_cast<std::uint64_t>(4096));
    TM_CHECK(!preferences.GetResumeOffset("terminal-b", &offset));
  }
  std::remove(path.c_str());
}

TM_TEST(Preferences, ResumeOffsetsNeverMoveBackwardsAndAreBounded) {
  Preferences preferences(TempPath("prefs_bounded.json"));
  preferences.SetResumeOffset("t", 100, 1);
  preferences.SetResumeOffset("t", 50, 2);
  std::uint64_t offset = 0;
  TM_CHECK(preferences.GetResumeOffset("t", &offset));
  TM_CHECK_EQ(offset, static_cast<std::uint64_t>(100));

  for (int i = 0; i < 200; ++i) {
    preferences.SetResumeOffset("terminal-" + std::to_string(i), 10,
                                static_cast<tmirror::Millis>(i));
  }
  // Bounded: a long-lived install must not accumulate an entry per terminal.
  int present = 0;
  for (int i = 0; i < 200; ++i) {
    if (preferences.GetResumeOffset("terminal-" + std::to_string(i), &offset)) ++present;
  }
  TM_CHECK(present <= static_cast<int>(Preferences::kMaxResumeEntries));
  // The most recent entries are the ones kept.
  TM_CHECK(preferences.GetResumeOffset("terminal-199", &offset));
}

TM_TEST(Preferences, UnreadableFileIsReportedNotFatal) {
  std::string path = TempPath("prefs_corrupt.json");
  std::FILE* file = std::fopen(path.c_str(), "wb");
  TM_REQUIRE(file != nullptr);
  std::fputs("{ this is not json", file);
  std::fclose(file);

  Preferences preferences(path);
  tmirror::Status status = preferences.Load();
  TM_CHECK(!status.ok());
  TM_CHECK(status.kind() == tmirror::ErrorKind::kStorageError);
  std::remove(path.c_str());
}
