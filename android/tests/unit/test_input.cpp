// Keyboard mapping under terminal modes and modifiers, paste handling, and the
// duplicate-event filter (spec §9, §16.1).

#include <string>

#include "framework.h"
#include "tm/input/key_encoder.h"
#include "tm/input/paste.h"
#include "tm/input/typed_text.h"
#include "tm/term/emulator.h"

using tmirror::Clock;
using tmirror::ManualClock;
using tmirror::input::DuplicateTextFilter;
using tmirror::input::EncodeMouseEvent;
using tmirror::input::Key;
using tmirror::input::KeyboardModes;
using tmirror::input::KeyEncoder;
using tmirror::input::KeyEvent;
using tmirror::input::kModAlt;
using tmirror::input::kModCtrl;
using tmirror::input::kModShift;
using tmirror::input::MouseAction;
using tmirror::input::MouseButton;
using tmirror::input::MouseEvent;
using tmirror::input::Paste;
using tmirror::input::PlanTypedText;
using tmirror::input::TypedTextPlan;
using tmirror::term::MouseEncoding;
using tmirror::term::MouseTracking;

namespace {

std::string Encode(Key key, std::uint8_t modifiers = 0, const KeyboardModes& modes = {}) {
  KeyEvent event;
  event.key = key;
  event.modifiers = modifiers;
  std::string out;
  if (!KeyEncoder::EncodeKey(event, modes, &out)) return "<none>";
  return out;
}

std::string EncodeChar(char32_t code_point, std::uint8_t modifiers = 0,
                       const KeyboardModes& modes = {}) {
  KeyEvent event;
  event.unicode = code_point;
  event.modifiers = modifiers;
  std::string out;
  if (!KeyEncoder::EncodeKey(event, modes, &out)) return "<none>";
  return out;
}

}  // namespace

TM_TEST(Input, PrintableCharactersAreUtf8) {
  TM_CHECK_EQ(EncodeChar(U'a'), "a");
  TM_CHECK_EQ(EncodeChar(0x00E9), "\xC3\xA9");
  TM_CHECK_EQ(EncodeChar(0x1F600), "\xF0\x9F\x98\x80");
}

TM_TEST(Input, CommittedTextCarriesNoModifiers) {
  KeyboardModes modes;
  // Committed text is sent verbatim (spec §9.1): there is nowhere in it to put a
  // modifier, and a latched Ctrl has to be applied by encoding the character as a *key*
  // instead. A soft keyboard delivers ordinary letters only as committed text, so a
  // platform layer that sends this path for a latched Ctrl makes Ctrl+C impossible from
  // the on-screen keyboard — there is then no way to interrupt a running program.
  TM_CHECK_EQ(KeyEncoder::EncodeText("c", modes), "c");
  TM_CHECK_EQ(EncodeChar(U'c', kModCtrl), std::string(1, '\x03'));
}

TM_TEST(Input, ControlCombinations) {
  TM_CHECK_EQ(EncodeChar(U'c', kModCtrl), std::string(1, '\x03'));
  TM_CHECK_EQ(EncodeChar(U'C', kModCtrl), std::string(1, '\x03'));
  TM_CHECK_EQ(EncodeChar(U'@', kModCtrl), std::string(1, '\0'));
  TM_CHECK_EQ(EncodeChar(U' ', kModCtrl), std::string(1, '\0'));
  TM_CHECK_EQ(EncodeChar(U'[', kModCtrl), "\x1b");
  TM_CHECK_EQ(EncodeChar(U'\\', kModCtrl), std::string(1, '\x1c'));
  TM_CHECK_EQ(EncodeChar(U'?', kModCtrl), std::string(1, '\x7f'));
  // A control combination with no defined byte falls back to the character itself.
  TM_CHECK_EQ(EncodeChar(U'\xE9', kModCtrl), "\xC3\xA9");
}

TM_TEST(Input, AltPrefixesWithEscape) {
  TM_CHECK_EQ(EncodeChar(U'x', kModAlt), "\x1bx");
  KeyboardModes modes;
  modes.alt_sends_escape = false;
  TM_CHECK_EQ(EncodeChar(U'x', kModAlt, modes), "x");
}

TM_TEST(Input, CursorKeysFollowApplicationMode) {
  KeyboardModes normal;
  KeyboardModes application;
  application.application_cursor = true;

  TM_CHECK_EQ(Encode(Key::kUp, 0, normal), "\x1b[A");
  TM_CHECK_EQ(Encode(Key::kUp, 0, application), "\x1bOA");
  TM_CHECK_EQ(Encode(Key::kLeft, 0, normal), "\x1b[D");
  TM_CHECK_EQ(Encode(Key::kHome, 0, normal), "\x1b[H");
  TM_CHECK_EQ(Encode(Key::kHome, 0, application), "\x1bOH");
  // A modified cursor key is always CSI with parameters, even in application mode.
  TM_CHECK_EQ(Encode(Key::kUp, kModCtrl, application), "\x1b[1;5A");
  TM_CHECK_EQ(Encode(Key::kRight, kModShift), "\x1b[1;2C");
}

TM_TEST(Input, EditingAndNavigationKeys) {
  TM_CHECK_EQ(Encode(Key::kInsert), "\x1b[2~");
  TM_CHECK_EQ(Encode(Key::kDelete), "\x1b[3~");
  TM_CHECK_EQ(Encode(Key::kPageUp), "\x1b[5~");
  TM_CHECK_EQ(Encode(Key::kPageDown), "\x1b[6~");
  TM_CHECK_EQ(Encode(Key::kDelete, kModCtrl), "\x1b[3;5~");
}

TM_TEST(Input, FunctionKeys) {
  TM_CHECK_EQ(Encode(Key::kF1), "\x1bOP");
  TM_CHECK_EQ(Encode(Key::kF4), "\x1bOS");
  TM_CHECK_EQ(Encode(Key::kF5), "\x1b[15~");
  TM_CHECK_EQ(Encode(Key::kF12), "\x1b[24~");
  TM_CHECK_EQ(Encode(Key::kF1, kModShift), "\x1b[1;2P");
  TM_CHECK_EQ(Encode(Key::kF5, kModCtrl), "\x1b[15;5~");
}

TM_TEST(Input, EnterTabAndBackspace) {
  KeyboardModes modes;
  TM_CHECK_EQ(Encode(Key::kEnter, 0, modes), "\r");
  modes.newline_mode = true;
  TM_CHECK_EQ(Encode(Key::kEnter, 0, modes), "\r\n");

  TM_CHECK_EQ(Encode(Key::kTab), "\t");
  TM_CHECK_EQ(Encode(Key::kTab, kModShift), "\x1b[Z");
  TM_CHECK_EQ(Encode(Key::kBackspace), std::string(1, '\x7f'));
  TM_CHECK_EQ(Encode(Key::kBackspace, kModCtrl), std::string(1, '\x08'));
  TM_CHECK_EQ(Encode(Key::kEscape), "\x1b");
}

TM_TEST(Input, KeypadFollowsApplicationKeypadMode) {
  KeyboardModes numeric;
  KeyboardModes application;
  application.application_keypad = true;

  TM_CHECK_EQ(Encode(Key::kKeypad5, 0, numeric), "5");
  TM_CHECK_EQ(Encode(Key::kKeypad5, 0, application), "\x1bOu");
  TM_CHECK_EQ(Encode(Key::kKeypadEnter, 0, numeric), "\r");
  TM_CHECK_EQ(Encode(Key::kKeypadEnter, 0, application), "\x1bOM");
  TM_CHECK_EQ(Encode(Key::kKeypadPlus, 0, application), "\x1bOk");
}

TM_TEST(Input, ModifierParameterFollowsXterm) {
  TM_CHECK_EQ(KeyEncoder::ModifierParameter(0), 1);
  TM_CHECK_EQ(KeyEncoder::ModifierParameter(kModShift), 2);
  TM_CHECK_EQ(KeyEncoder::ModifierParameter(kModAlt), 3);
  TM_CHECK_EQ(KeyEncoder::ModifierParameter(kModCtrl), 5);
  TM_CHECK_EQ(KeyEncoder::ModifierParameter(kModShift | kModCtrl), 6);
}

TM_TEST(Input, TextCommitsNormaliseNewlines) {
  KeyboardModes modes;
  TM_CHECK_EQ(KeyEncoder::EncodeText("a\nb", modes), "a\rb");
  TM_CHECK_EQ(KeyEncoder::EncodeText("a\r\nb", modes), "a\rb");
  TM_CHECK_EQ(KeyEncoder::EncodeText("héllo", modes), "héllo");
}

TM_TEST(Input, FocusReportingOnlyWhenEnabled) {
  TM_CHECK_EQ(KeyEncoder::EncodeFocus(true, false), "");
  TM_CHECK_EQ(KeyEncoder::EncodeFocus(true, true), "\x1b[I");
  TM_CHECK_EQ(KeyEncoder::EncodeFocus(false, true), "\x1b[O");
}

TM_TEST(Input, DuplicateTextIsSuppressedInsideTheWindow) {
  ManualClock clock;
  DuplicateTextFilter filter(&clock);
  filter.RecordKeyBytes("a");
  TM_CHECK(filter.ShouldSuppressText("a"));
  // Only once: a genuine second press must still get through.
  TM_CHECK(!filter.ShouldSuppressText("a"));

  filter.RecordKeyBytes("b");
  clock.Advance(500);
  TM_CHECK(!filter.ShouldSuppressText("b"));

  filter.RecordKeyBytes("c");
  TM_CHECK(!filter.ShouldSuppressText("d"));
}

TM_TEST(Paste, NormalisesLineEndings) {
  Paste::Options options;
  TM_CHECK_EQ(Paste::Normalize("a\r\nb\nc\rd", options), "a\rb\rc\rd");
}

TM_TEST(Paste, StripsControlsWhenNotBracketed) {
  Paste::Options options;
  options.bracketed = false;
  TM_CHECK_EQ(Paste::Normalize("safe\x1b[31mtext", options), "safe[31mtext");

  options.bracketed = true;
  TM_CHECK_EQ(Paste::Normalize("safe\x1b[31mtext", options), "safe\x1b[31mtext");
}

TM_TEST(Paste, WrapsAndChunksBracketedPaste) {
  Paste::Options options;
  options.bracketed = true;
  options.chunk_bytes = 4;
  bool too_large = false;
  std::vector<std::string> chunks = Paste::Prepare("abcdefghij", options, &too_large);
  TM_CHECK(!too_large);
  TM_REQUIRE(chunks.size() >= 3);
  TM_CHECK_EQ(chunks.front(), Paste::kBracketStart);
  TM_CHECK_EQ(chunks.back(), Paste::kBracketEnd);
  std::string joined;
  for (std::size_t i = 1; i + 1 < chunks.size(); ++i) joined += chunks[i];
  TM_CHECK_EQ(joined, "abcdefghij");
}

TM_TEST(Paste, NeverSplitsAUtf8Sequence) {
  Paste::Options options;
  options.chunk_bytes = 3;
  bool too_large = false;
  // Four two-byte characters: a naive split at three bytes would cut one in half.
  std::vector<std::string> chunks = Paste::Prepare("ééééé", options, &too_large);
  for (const std::string& chunk : chunks) {
    unsigned char last = static_cast<unsigned char>(chunk.back());
    TM_CHECK((last & 0xC0) != 0xC0);  // never ends on a lead byte
  }
  std::string joined;
  for (const std::string& chunk : chunks) joined += chunk;
  TM_CHECK_EQ(joined, "ééééé");
}

TM_TEST(Paste, RefusesOversizedInput) {
  Paste::Options options;
  options.max_bytes = 16;
  bool too_large = false;
  std::vector<std::string> chunks = Paste::Prepare(std::string(100, 'x'), options, &too_large);
  TM_CHECK(too_large);
  TM_CHECK(chunks.empty());
}

TM_TEST(Mouse, SgrEncodingDistinguishesPressAndRelease) {
  MouseEvent event;
  event.button = MouseButton::kLeft;
  event.action = MouseAction::kPress;
  event.column = 10;
  event.row = 4;
  std::string out;
  TM_CHECK(EncodeMouseEvent(event, MouseTracking::kNormal, MouseEncoding::kSgr, &out));
  TM_CHECK_EQ(out, "\x1b[<0;11;5M");

  event.action = MouseAction::kRelease;
  TM_CHECK(EncodeMouseEvent(event, MouseTracking::kNormal, MouseEncoding::kSgr, &out));
  TM_CHECK_EQ(out, "\x1b[<0;11;5m");
}

TM_TEST(Mouse, NothingIsSentWhenTrackingIsOff) {
  MouseEvent event;
  std::string out;
  TM_CHECK(!EncodeMouseEvent(event, MouseTracking::kOff, MouseEncoding::kSgr, &out));
}

TM_TEST(Mouse, X10EncodingRefusesCoordinatesItCannotExpress) {
  MouseEvent event;
  event.column = 500;
  event.row = 1;
  std::string out;
  TM_CHECK(!EncodeMouseEvent(event, MouseTracking::kNormal, MouseEncoding::kX10, &out));
  event.column = 10;
  TM_CHECK(EncodeMouseEvent(event, MouseTracking::kNormal, MouseEncoding::kX10, &out));
  TM_CHECK_EQ(out.size(), static_cast<std::size_t>(6));
}


// ------------------------------------------- a latched modifier meeting typed text
//
// A soft keyboard delivers letters as committed text, and a modifier cannot travel in
// committed text — so this split is the only way a latched Ctrl reaches the terminal.
// It lives in core precisely so it can be proved here, on a developer machine, rather
// than only on a phone with a particular keyboard.

namespace {

TypedTextPlan Plan(const std::string& pending, const std::string& value,
                   std::uint8_t latched) {
  return PlanTypedText(tmirror::ByteView(pending), tmirror::ByteView(value), latched);
}

}  // namespace

TM_TEST(Input, ALatchedControlTurnsTheNextCharacterIntoAKeypress) {
  // The reported bug, reduced: Ctrl armed, nothing composing, the user taps "c".
  TypedTextPlan plan = Plan("", "c", kModCtrl);
  TM_CHECK(plan.has_key);
  TM_CHECK_EQ(static_cast<int>(plan.unicode), static_cast<int>(U'c'));
  TM_CHECK_EQ(static_cast<int>(plan.modifiers), static_cast<int>(kModCtrl));
  TM_CHECK(plan.consumes_latch);
  TM_CHECK(plan.leading.empty());
  TM_CHECK(plan.trailing.empty());

  // ...and that pair is what the encoder turns into the interrupt byte.
  TM_CHECK_EQ(EncodeChar(plan.unicode, plan.modifiers), std::string(1, '\x03'));
}

TM_TEST(Input, TypedTextWithNoLatchIsJustText) {
  TypedTextPlan plan = Plan("", "hello", 0);
  TM_CHECK(!plan.has_key);
  TM_CHECK(!plan.consumes_latch);
  TM_CHECK_EQ(plan.trailing, std::string("hello"));
}

TM_TEST(Input, AModifierArmedMidWordLeavesTheWordUnmodified) {
  // The user was typing "gi", armed Ctrl, then pressed "t". Only what the update *adds*
  // is the control key; the letters already typed are ordinary text and go first.
  TypedTextPlan plan = Plan("gi", "git", kModCtrl);
  TM_CHECK_EQ(plan.leading, std::string("gi"));
  TM_CHECK(plan.has_key);
  TM_CHECK_EQ(static_cast<int>(plan.unicode), static_cast<int>(U't'));
  TM_CHECK(plan.trailing.empty());
}

TM_TEST(Input, AShrinkingCompositionIsAnEditAndNotAKeypress) {
  // Backspace during composition: the keyboard recomposes "gi" as "g". Reading that as
  // a keypress would send Ctrl+G — a bell — for what the user pressed as Backspace, and
  // throw the rest of the word away. The latch stays armed for the next real key.
  TypedTextPlan plan = Plan("gi", "g", kModCtrl);
  TM_CHECK(!plan.has_key);
  TM_CHECK(!plan.consumes_latch);
  TM_CHECK(plan.leading.empty());
  TM_CHECK(plan.trailing.empty());
}

TM_TEST(Input, AReplacedCompositionIsAnEditAndNotAKeypress) {
  // Autocorrect or a swipe replaces the whole composition rather than extending it.
  TypedTextPlan plan = Plan("teh", "the", kModCtrl);
  TM_CHECK(!plan.has_key);
  TM_CHECK(!plan.consumes_latch);
}

TM_TEST(Input, TheLatchCoversExactlyOneCharacter) {
  // A keyboard that commits several characters at once still spends the modifier on the
  // first, exactly as a hardware key would.
  TypedTextPlan plan = Plan("", "abc", kModCtrl);
  TM_CHECK(plan.has_key);
  TM_CHECK_EQ(static_cast<int>(plan.unicode), static_cast<int>(U'a'));
  TM_CHECK_EQ(plan.trailing, std::string("bc"));
}

TM_TEST(Input, AlsoWorksForAltWhichSendsAnEscapePrefix) {
  TypedTextPlan plan = Plan("", "f", kModAlt);
  TM_CHECK(plan.has_key);
  KeyboardModes modes;
  modes.alt_sends_escape = true;
  TM_CHECK_EQ(EncodeChar(plan.unicode, plan.modifiers, modes), std::string("\x1b") + "f");
}

TM_TEST(Input, TheSplitNeverCutsAMultiByteCharacterInHalf) {
  // "é" is two bytes; a byte-wise prefix test that was not UTF-8 safe would slice it.
  TypedTextPlan plan = Plan("caf\xC3\xA9", "caf\xC3\xA9s", kModCtrl);
  TM_CHECK_EQ(plan.leading, std::string("caf\xC3\xA9"));
  TM_CHECK(plan.has_key);
  TM_CHECK_EQ(static_cast<int>(plan.unicode), static_cast<int>(U's'));

  // And a non-ASCII character can itself be the modified one.
  TypedTextPlan accented = Plan("", "\xC3\xA9", kModCtrl);
  TM_CHECK(accented.has_key);
  TM_CHECK_EQ(static_cast<int>(accented.unicode), 0xE9);
}

TM_TEST(Input, AnEmptyDeliveryDoesNothingAndKeepsTheLatch) {
  TypedTextPlan plan = Plan("", "", kModCtrl);
  TM_CHECK(!plan.has_key);
  TM_CHECK(!plan.consumes_latch);
  TM_CHECK(plan.trailing.empty());
}
