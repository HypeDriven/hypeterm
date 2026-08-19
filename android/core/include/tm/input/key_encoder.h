#pragma once

#include <cstdint>
#include <string>

#include "tm/term/emulator.h"
#include "tm/util/time.h"

namespace tmirror {
namespace input {

/// Named keys that do not have a direct Unicode representation. Printable keys carry
/// their code point in `KeyEvent::unicode` instead.
enum class Key {
  kNone,
  kEnter,
  kTab,
  kBackspace,
  kEscape,
  kDelete,
  kInsert,
  kHome,
  kEnd,
  kPageUp,
  kPageDown,
  kUp,
  kDown,
  kLeft,
  kRight,
  kF1, kF2, kF3, kF4, kF5, kF6, kF7, kF8, kF9, kF10, kF11, kF12,
  kF13, kF14, kF15, kF16, kF17, kF18, kF19, kF20,
  kKeypad0, kKeypad1, kKeypad2, kKeypad3, kKeypad4,
  kKeypad5, kKeypad6, kKeypad7, kKeypad8, kKeypad9,
  kKeypadEnter,
  kKeypadPlus,
  kKeypadMinus,
  kKeypadMultiply,
  kKeypadDivide,
  kKeypadDecimal,
  kKeypadComma,
  kKeypadEquals,
};

enum KeyModifier : std::uint8_t {
  kModNone = 0,
  kModShift = 1u << 0,
  kModAlt = 1u << 1,
  kModCtrl = 1u << 2,
  kModMeta = 1u << 3,
};

struct KeyEvent {
  Key key = Key::kNone;
  /// Code point produced by the platform for a printable key, or 0.
  char32_t unicode = 0;
  std::uint8_t modifiers = kModNone;
  bool repeat = false;
  /// Platform key code, carried through for duplicate filtering only.
  std::int32_t platform_key_code = 0;
};

/// Terminal modes that change what a key sends. Snapshotted from the emulator so the
/// encoder never reaches across threads into live terminal state.
struct KeyboardModes {
  bool application_cursor = false;
  bool application_keypad = false;
  bool newline_mode = false;
  bool bracketed_paste = false;
  /// Initial policy from spec §9.2: Alt prefixes the key with ESC.
  bool alt_sends_escape = true;
  /// Backspace sends DEL (0x7F) like xterm; some hosts prefer BS (0x08).
  bool backspace_sends_delete = true;
};

/// Translates platform key and text events into the bytes a VT terminal expects
/// (spec §9.2). Pure function of (event, modes) — no I/O, no allocation beyond the
/// returned string.
class KeyEncoder {
 public:
  /// Returns false when the key produces nothing (a bare modifier, an unmapped key).
  static bool EncodeKey(const KeyEvent& event, const KeyboardModes& modes, std::string* out);

  /// Committed IME text (spec §9.1). Composition is never sent; only commits are.
  static std::string EncodeText(const std::string& utf8_text, const KeyboardModes& modes);

  /// Focus reporting (DECSET 1004); empty when the mode is off.
  static std::string EncodeFocus(bool focused, bool focus_reporting_enabled);

  /// xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl + 8*meta.
  static int ModifierParameter(std::uint8_t modifiers);

  /// The control byte for Ctrl+<key>, or -1 when the combination has none.
  static int ControlByteFor(char32_t code_point);
};

/// Android delivers a hardware key press and an IME `commitText` for the same
/// character often enough that sending both is a real bug: the shell would see it
/// twice. Spec §9.2 requires filtering those duplicates.
///
/// The filter is deliberately narrow: it drops a text commit that exactly matches
/// bytes produced by a key event within a short window, and nothing else.
class DuplicateTextFilter {
 public:
  explicit DuplicateTextFilter(Clock* clock = Clock::System()) : clock_(clock) {}

  void RecordKeyBytes(const std::string& bytes);
  /// True when this commit should be suppressed.
  bool ShouldSuppressText(const std::string& utf8_text);
  void Reset();

  void set_window_ms(Millis window) { window_ms_ = window; }

 private:
  Clock* clock_;
  std::string last_key_bytes_;
  Millis last_key_time_ = -1;
  Millis window_ms_ = 60;
};

/// Mouse reporting (spec §8.1: supported when the UI enables those inputs).
enum class MouseButton { kLeft = 0, kMiddle = 1, kRight = 2, kNone = 3, kWheelUp = 64, kWheelDown = 65 };
enum class MouseAction { kPress, kRelease, kMove };

struct MouseEvent {
  MouseButton button = MouseButton::kLeft;
  MouseAction action = MouseAction::kPress;
  int column = 0;  // 0-based cell coordinates
  int row = 0;
  std::uint8_t modifiers = kModNone;
};

/// Encodes a mouse event for the active tracking mode, or returns false when the
/// terminal has not asked for that class of event.
bool EncodeMouseEvent(const MouseEvent& event, term::MouseTracking tracking,
                      term::MouseEncoding encoding, std::string* out);

}  // namespace input
}  // namespace tmirror
