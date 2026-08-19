#include "tm/input/key_encoder.h"

#include "tm/term/utf8.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace input {
namespace {

/// `CSI <number> ; <mod> ~` or `CSI <number> ~` when unmodified.
std::string TildeSequence(int number, int modifier) {
  std::string out = "\x1b[";
  out += Int64ToString(number);
  if (modifier > 1) {
    out += ";";
    out += Int64ToString(modifier);
  }
  out += "~";
  return out;
}

/// Cursor and function keys share a shape: SS3 when the terminal is in application
/// mode and unmodified, CSI otherwise. A modified key is always CSI with parameters,
/// which is what xterm does and what terminfo entries expect.
std::string CursorSequence(char final_byte, int modifier, bool application) {
  std::string out;
  if (modifier > 1) {
    out = "\x1b[1;";
    out += Int64ToString(modifier);
    out += final_byte;
  } else if (application) {
    out = "\x1bO";
    out += final_byte;
  } else {
    out = "\x1b[";
    out += final_byte;
  }
  return out;
}

bool IsKeypad(Key key) {
  return key >= Key::kKeypad0 && key <= Key::kKeypadEquals;
}

/// Application-keypad (DECKPAM) byte for a keypad key.
char KeypadApplicationByte(Key key) {
  switch (key) {
    case Key::kKeypad0: return 'p';
    case Key::kKeypad1: return 'q';
    case Key::kKeypad2: return 'r';
    case Key::kKeypad3: return 's';
    case Key::kKeypad4: return 't';
    case Key::kKeypad5: return 'u';
    case Key::kKeypad6: return 'v';
    case Key::kKeypad7: return 'w';
    case Key::kKeypad8: return 'x';
    case Key::kKeypad9: return 'y';
    case Key::kKeypadEnter: return 'M';
    case Key::kKeypadPlus: return 'k';
    case Key::kKeypadMinus: return 'm';
    case Key::kKeypadMultiply: return 'j';
    case Key::kKeypadDivide: return 'o';
    case Key::kKeypadDecimal: return 'n';
    case Key::kKeypadComma: return 'l';
    case Key::kKeypadEquals: return 'X';
    default: return 0;
  }
}

char KeypadNumericChar(Key key) {
  switch (key) {
    case Key::kKeypad0: return '0';
    case Key::kKeypad1: return '1';
    case Key::kKeypad2: return '2';
    case Key::kKeypad3: return '3';
    case Key::kKeypad4: return '4';
    case Key::kKeypad5: return '5';
    case Key::kKeypad6: return '6';
    case Key::kKeypad7: return '7';
    case Key::kKeypad8: return '8';
    case Key::kKeypad9: return '9';
    case Key::kKeypadPlus: return '+';
    case Key::kKeypadMinus: return '-';
    case Key::kKeypadMultiply: return '*';
    case Key::kKeypadDivide: return '/';
    case Key::kKeypadDecimal: return '.';
    case Key::kKeypadComma: return ',';
    case Key::kKeypadEquals: return '=';
    default: return 0;
  }
}

int FunctionKeyTildeNumber(Key key) {
  switch (key) {
    case Key::kF5: return 15;
    case Key::kF6: return 17;
    case Key::kF7: return 18;
    case Key::kF8: return 19;
    case Key::kF9: return 20;
    case Key::kF10: return 21;
    case Key::kF11: return 23;
    case Key::kF12: return 24;
    case Key::kF13: return 25;
    case Key::kF14: return 26;
    case Key::kF15: return 28;
    case Key::kF16: return 29;
    case Key::kF17: return 31;
    case Key::kF18: return 32;
    case Key::kF19: return 33;
    case Key::kF20: return 34;
    default: return 0;
  }
}

}  // namespace

int KeyEncoder::ModifierParameter(std::uint8_t modifiers) {
  int value = 1;
  if (modifiers & kModShift) value += 1;
  if (modifiers & kModAlt) value += 2;
  if (modifiers & kModCtrl) value += 4;
  if (modifiers & kModMeta) value += 8;
  return value;
}

int KeyEncoder::ControlByteFor(char32_t code_point) {
  if (code_point >= U'a' && code_point <= U'z') {
    return static_cast<int>(code_point - U'a' + 1);
  }
  if (code_point >= U'A' && code_point <= U'Z') {
    return static_cast<int>(code_point - U'A' + 1);
  }
  switch (code_point) {
    case U' ':
    case U'@':
    case U'2': return 0x00;
    case U'[': return 0x1B;
    case U'\\':
    case U'4': return 0x1C;
    case U']':
    case U'5': return 0x1D;
    case U'^':
    case U'6': return 0x1E;
    case U'_':
    case U'7':
    case U'/': return 0x1F;
    case U'3': return 0x1B;
    case U'8':
    case U'?': return 0x7F;
    default: return -1;
  }
}

bool KeyEncoder::EncodeKey(const KeyEvent& event, const KeyboardModes& modes,
                           std::string* out) {
  out->clear();
  const int modifier = ModifierParameter(event.modifiers);
  const bool ctrl = (event.modifiers & kModCtrl) != 0;
  const bool alt = (event.modifiers & kModAlt) != 0;
  const bool shift = (event.modifiers & kModShift) != 0;
  const bool alt_prefix = alt && modes.alt_sends_escape;

  std::string body;
  bool handled = true;

  switch (event.key) {
    case Key::kEnter:
      body = modes.newline_mode ? "\r\n" : "\r";
      break;
    case Key::kTab:
      if (shift) {
        body = "\x1b[Z";  // CBT
      } else if (modifier > 1) {
        body = "\x1b[9;" + Int64ToString(modifier) + "u";
      } else {
        body = "\t";
      }
      break;
    case Key::kBackspace:
      if (ctrl) {
        body = modes.backspace_sends_delete ? std::string(1, '\x08') : std::string(1, '\x7f');
      } else {
        body = modes.backspace_sends_delete ? std::string(1, '\x7f') : std::string(1, '\x08');
      }
      break;
    case Key::kEscape:
      body = "\x1b";
      break;
    case Key::kUp:
      body = CursorSequence('A', modifier, modes.application_cursor);
      break;
    case Key::kDown:
      body = CursorSequence('B', modifier, modes.application_cursor);
      break;
    case Key::kRight:
      body = CursorSequence('C', modifier, modes.application_cursor);
      break;
    case Key::kLeft:
      body = CursorSequence('D', modifier, modes.application_cursor);
      break;
    case Key::kHome:
      body = CursorSequence('H', modifier, modes.application_cursor);
      break;
    case Key::kEnd:
      body = CursorSequence('F', modifier, modes.application_cursor);
      break;
    case Key::kInsert:
      body = TildeSequence(2, modifier);
      break;
    case Key::kDelete:
      body = TildeSequence(3, modifier);
      break;
    case Key::kPageUp:
      body = TildeSequence(5, modifier);
      break;
    case Key::kPageDown:
      body = TildeSequence(6, modifier);
      break;
    case Key::kF1:
      body = CursorSequence('P', modifier, true);
      break;
    case Key::kF2:
      body = CursorSequence('Q', modifier, true);
      break;
    case Key::kF3:
      body = CursorSequence('R', modifier, true);
      break;
    case Key::kF4:
      body = CursorSequence('S', modifier, true);
      break;
    default:
      handled = false;
      break;
  }

  if (!handled) {
    int tilde = FunctionKeyTildeNumber(event.key);
    if (tilde != 0) {
      body = TildeSequence(tilde, modifier);
      handled = true;
    }
  }

  if (!handled && IsKeypad(event.key)) {
    if (event.key == Key::kKeypadEnter) {
      body = modes.application_keypad ? std::string("\x1bOM")
                                      : (modes.newline_mode ? std::string("\r\n")
                                                            : std::string("\r"));
    } else if (modes.application_keypad) {
      char final_byte = KeypadApplicationByte(event.key);
      body = std::string("\x1bO") + final_byte;
    } else {
      char literal = KeypadNumericChar(event.key);
      if (literal != 0) body = std::string(1, literal);
    }
    handled = !body.empty();
  }

  if (!handled) {
    // A printable key: the platform already applied shift and any dead-key
    // composition, so the code point is authoritative.
    if (event.unicode == 0) return false;
    if (ctrl) {
      int control = ControlByteFor(event.unicode);
      if (control >= 0) {
        body = std::string(1, static_cast<char>(control));
      } else {
        body = term::EncodeUtf8(event.unicode);
      }
    } else {
      body = term::EncodeUtf8(event.unicode);
    }
  }

  if (body.empty()) return false;
  if (alt_prefix && (body.size() == 1 || body[0] != '\x1b')) {
    // ESC-prefixing an escape sequence would corrupt it; only plain bytes get it.
    *out = "\x1b" + body;
  } else {
    *out = body;
  }
  return true;
}

std::string KeyEncoder::EncodeText(const std::string& utf8_text, const KeyboardModes& modes) {
  (void)modes;
  // Committed text is sent verbatim as UTF-8 (spec §9.1). Newlines a soft keyboard
  // may include are normalised to CR, which is what a PTY expects from Enter.
  std::string out;
  out.reserve(utf8_text.size());
  for (std::size_t i = 0; i < utf8_text.size(); ++i) {
    char c = utf8_text[i];
    if (c == '\n') {
      out.push_back('\r');
    } else if (c == '\r') {
      out.push_back('\r');
      if (i + 1 < utf8_text.size() && utf8_text[i + 1] == '\n') ++i;
    } else {
      out.push_back(c);
    }
  }
  return out;
}

std::string KeyEncoder::EncodeFocus(bool focused, bool focus_reporting_enabled) {
  if (!focus_reporting_enabled) return std::string();
  return focused ? "\x1b[I" : "\x1b[O";
}

void DuplicateTextFilter::RecordKeyBytes(const std::string& bytes) {
  last_key_bytes_ = bytes;
  last_key_time_ = clock_->MonotonicMillis();
}

bool DuplicateTextFilter::ShouldSuppressText(const std::string& utf8_text) {
  if (last_key_time_ < 0 || last_key_bytes_.empty()) return false;
  if (clock_->MonotonicMillis() - last_key_time_ > window_ms_) return false;
  if (last_key_bytes_ != utf8_text) return false;
  // One suppression per key event: a genuine repeat typed inside the window must
  // still reach the terminal.
  last_key_bytes_.clear();
  last_key_time_ = -1;
  return true;
}

void DuplicateTextFilter::Reset() {
  last_key_bytes_.clear();
  last_key_time_ = -1;
}

bool EncodeMouseEvent(const MouseEvent& event, term::MouseTracking tracking,
                      term::MouseEncoding encoding, std::string* out) {
  out->clear();
  if (tracking == term::MouseTracking::kOff) return false;
  if (event.action == MouseAction::kMove && tracking != term::MouseTracking::kAnyEvent &&
      tracking != term::MouseTracking::kButtonEvent) {
    return false;
  }
  if (event.action == MouseAction::kRelease && tracking == term::MouseTracking::kX10) {
    return false;
  }

  int button = static_cast<int>(event.button);
  if (event.action == MouseAction::kMove) button += 32;
  if (event.modifiers & kModShift) button += 4;
  if (event.modifiers & kModAlt) button += 8;
  if (event.modifiers & kModCtrl) button += 16;

  const int column = event.column + 1;
  const int row = event.row + 1;

  if (encoding == term::MouseEncoding::kSgr) {
    int sgr_button = button;
    if (event.action == MouseAction::kRelease) {
      // SGR reports the button on release too, distinguished by the final 'm'.
      *out = "\x1b[<" + Int64ToString(sgr_button) + ";" + Int64ToString(column) + ";" +
             Int64ToString(row) + "m";
    } else {
      *out = "\x1b[<" + Int64ToString(sgr_button) + ";" + Int64ToString(column) + ";" +
             Int64ToString(row) + "M";
    }
    return true;
  }

  if (event.action == MouseAction::kRelease) button = 3 + (button & ~3);

  if (encoding == term::MouseEncoding::kUrxvt) {
    *out = "\x1b[" + Int64ToString(button + 32) + ";" + Int64ToString(column) + ";" +
           Int64ToString(row) + "M";
    return true;
  }

  // X10 and UTF-8 encodings cannot express coordinates past their limits; the event
  // is dropped rather than reported at a wrong position.
  if (encoding == term::MouseEncoding::kUtf8) {
    if (column > 2015 || row > 2015) return false;
    std::string body = "\x1b[M";
    term::AppendUtf8(static_cast<char32_t>(button + 32), &body);
    term::AppendUtf8(static_cast<char32_t>(column + 32), &body);
    term::AppendUtf8(static_cast<char32_t>(row + 32), &body);
    *out = body;
    return true;
  }

  if (column > 223 || row > 223) return false;
  std::string body = "\x1b[M";
  body.push_back(static_cast<char>(button + 32));
  body.push_back(static_cast<char>(column + 32));
  body.push_back(static_cast<char>(row + 32));
  *out = body;
  return true;
}

}  // namespace input
}  // namespace tmirror
