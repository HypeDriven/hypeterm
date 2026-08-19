#include "tm/term/emulator.h"

#include <algorithm>
#include <utility>

#include "tm/term/width.h"
#include "tm/util/base64.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace term {
namespace {

/// DEC Special Graphics (ESC ( 0). Box drawing is not optional in practice: vim,
/// tmux, dialog and ncurses all use it for frames.
char32_t DecSpecialGraphic(char32_t c) {
  switch (c) {
    case U'`': return 0x25C6;  // diamond
    case U'a': return 0x2592;  // checkerboard
    case U'b': return 0x2409;  // HT
    case U'c': return 0x240C;  // FF
    case U'd': return 0x240D;  // CR
    case U'e': return 0x240A;  // LF
    case U'f': return 0x00B0;  // degree
    case U'g': return 0x00B1;  // plus/minus
    case U'h': return 0x2424;  // NL
    case U'i': return 0x240B;  // VT
    case U'j': return 0x2518;
    case U'k': return 0x2510;
    case U'l': return 0x250C;
    case U'm': return 0x2514;
    case U'n': return 0x253C;
    case U'o': return 0x23BA;
    case U'p': return 0x23BB;
    case U'q': return 0x2500;
    case U'r': return 0x23BC;
    case U's': return 0x23BD;
    case U't': return 0x251C;
    case U'u': return 0x2524;
    case U'v': return 0x2534;
    case U'w': return 0x252C;
    case U'x': return 0x2502;
    case U'y': return 0x2264;
    case U'z': return 0x2265;
    case U'{': return 0x03C0;
    case U'|': return 0x2260;
    case U'}': return 0x00A3;
    case U'~': return 0x00B7;
    default: return c;
  }
}

struct LogicalLine {
  std::vector<Cell> cells;
  std::vector<std::pair<std::size_t, std::u32string>> marks;
};

}  // namespace

Emulator::Emulator(const EmulatorConfig& config)
    : config_(config),
      scrollback_(config.scrollback),
      primary_(config.columns, config.rows, &scrollback_),
      alt_(config.columns, config.rows, nullptr),
      parser_(this) {
  parser_.SetLimits(config.parser);
}

void Emulator::Feed(ByteView bytes) { parser_.Feed(bytes); }

std::uint64_t Emulator::revision() const {
  return primary_.revision() + alt_.revision() + scrollback_.revision() + mode_revision_;
}

void Emulator::Reset() {
  parser_.Reset();
  scrollback_.ClearScrollback();
  primary_.Reset();
  alt_.Reset();
  alt_screen_active_ = false;
  application_cursor_keys_ = false;
  application_keypad_ = false;
  bracketed_paste_ = false;
  focus_reporting_ = false;
  newline_mode_ = false;
  cursor_visible_ = true;
  cursor_blinking_ = true;
  cursor_shape_ = CursorShape::kBlock;
  mouse_tracking_ = MouseTracking::kOff;
  mouse_encoding_ = MouseEncoding::kX10;
  charset_g0_ = 'B';
  charset_g1_ = 'B';
  active_charset_ = 0;
  alt_saved_cursor_ = SavedCursorState();
  title_.clear();
  ++mode_revision_;
  if (title_callback_) title_callback_(title_);
}

void Emulator::SendResponse(const std::string& bytes) {
  if (response_sink_ && !bytes.empty()) {
    response_sink_(ByteView::FromChars(bytes.data(), bytes.size()));
  }
}

char32_t Emulator::TranslateCharset(char32_t code_point) const {
  int charset = active_charset_ == 0 ? charset_g0_ : charset_g1_;
  if (charset == '0' && code_point >= 0x60 && code_point <= 0x7E) {
    return DecSpecialGraphic(code_point);
  }
  return code_point;
}

void Emulator::OnPrint(char32_t code_point) {
  // Controls never reach the screen as text; the parser dispatches them, and a
  // replacement produced mid-sequence must not become a combining mark.
  if (code_point < 0x20 || code_point == 0x7F) return;

  char32_t translated = TranslateCharset(code_point);
  int width = CharWidth(translated);
  if (width == 0) {
    active().AddCombiningMark(translated);
    return;
  }
  active().PutChar(translated, width);
}

void Emulator::OnExecute(std::uint8_t control) {
  Screen& screen = active();
  switch (control) {
    case 0x07:  // BEL
      if (bell_callback_) bell_callback_();
      break;
    case 0x08:  // BS
      screen.Backspace();
      break;
    case 0x09:  // HT
      screen.Tab(1);
      break;
    case 0x0A:  // LF
    case 0x0B:  // VT
    case 0x0C:  // FF
      screen.LineFeed();
      if (newline_mode_) screen.CarriageReturn();
      break;
    case 0x0D:  // CR
      screen.CarriageReturn();
      break;
    case 0x0E:  // SO — shift out to G1
      active_charset_ = 1;
      ++mode_revision_;
      break;
    case 0x0F:  // SI — shift in to G0
      active_charset_ = 0;
      ++mode_revision_;
      break;
    default:
      // NUL and every other control is ignored, which is what a real terminal does.
      break;
  }
}

void Emulator::OnEscape(const std::string& intermediates, std::uint8_t final_byte) {
  Screen& screen = active();
  if (!intermediates.empty()) {
    HandleDecSequence(intermediates, final_byte);
    return;
  }
  switch (final_byte) {
    case 'D':  // IND
      screen.LineFeed();
      break;
    case 'E':  // NEL
      screen.CarriageReturn();
      screen.LineFeed();
      break;
    case 'H':  // HTS
      screen.SetTabStop();
      break;
    case 'M':  // RI
      screen.ReverseIndex();
      break;
    case 'Z':  // DECID, an obsolete alias for DA1
      if (config_.answer_device_queries) SendResponse("\x1b[?62;22c");
      break;
    case '7':  // DECSC
      screen.SaveCursor();
      break;
    case '8':  // DECRC
      screen.RestoreCursor();
      break;
    case '=':  // DECKPAM
      application_keypad_ = true;
      ++mode_revision_;
      break;
    case '>':  // DECKPNM
      application_keypad_ = false;
      ++mode_revision_;
      break;
    case 'c':  // RIS
      Reset();
      break;
    case 'n':  // LS2
    case 'o':  // LS3
    case '\\':  // ST
    default:
      break;
  }
}

void Emulator::HandleDecSequence(const std::string& intermediates, std::uint8_t final_byte) {
  char intermediate = intermediates[0];
  switch (intermediate) {
    case '(':
      charset_g0_ = final_byte;
      ++mode_revision_;
      break;
    case ')':
      charset_g1_ = final_byte;
      ++mode_revision_;
      break;
    case '*':
    case '+':
      // G2/G3 are accepted and ignored; nothing in the supported profile selects them.
      break;
    case '#':
      if (final_byte == '8') active().FillScreen(U'E');  // DECALN
      break;
    default:
      break;
  }
}

void Emulator::SetCursorStyle(int style) {
  switch (style) {
    case 0:
    case 1:
      cursor_shape_ = CursorShape::kBlock;
      cursor_blinking_ = true;
      break;
    case 2:
      cursor_shape_ = CursorShape::kBlock;
      cursor_blinking_ = false;
      break;
    case 3:
      cursor_shape_ = CursorShape::kUnderline;
      cursor_blinking_ = true;
      break;
    case 4:
      cursor_shape_ = CursorShape::kUnderline;
      cursor_blinking_ = false;
      break;
    case 5:
      cursor_shape_ = CursorShape::kBar;
      cursor_blinking_ = true;
      break;
    case 6:
      cursor_shape_ = CursorShape::kBar;
      cursor_blinking_ = false;
      break;
    default:
      return;
  }
  ++mode_revision_;
}

void Emulator::OnCsi(const Params& params, std::uint8_t final_byte) {
  Screen& screen = active();
  const std::string& intermediates = params.intermediates();

  if (params.prefix() == '?') {
    switch (final_byte) {
      case 'h':
        for (int i = 0; i < params.count(); ++i) SetPrivateMode(params.Get(i, 0), true);
        return;
      case 'l':
        for (int i = 0; i < params.count(); ++i) SetPrivateMode(params.Get(i, 0), false);
        return;
      case 'n':  // DECDSR
        if (config_.answer_device_queries && params.Get(0, 0) == 6) {
          SendResponse("\x1b[?" + Int64ToString(screen.cursor_row() + 1) + ";" +
                       Int64ToString(screen.cursor_column() + 1) + ";1R");
        }
        return;
      case 'p':  // DECRQM (private) when the intermediate is '$'
        if (config_.answer_device_queries && intermediates == "$") {
          int mode = params.Get(0, 0);
          SendResponse("\x1b[?" + Int64ToString(mode) + ";" +
                       Int64ToString(PrivateModeState(mode)) + "$y");
        }
        return;
      case 'q':  // DECSCUSR is not private; '?' here is a query we do not answer
      default:
        return;
    }
  }

  if (params.prefix() == '>') {
    if (final_byte == 'c' && config_.answer_device_queries) {
      // Secondary DA: terminal type 0, firmware version, no cartridge.
      SendResponse("\x1b[>0;276;0c");
    }
    // modifyOtherKeys and XTVERSION queries are accepted and ignored.
    return;
  }

  if (params.prefix() == '=') {
    // Tertiary DA and friends: ignored.
    return;
  }

  switch (final_byte) {
    case '@':
      screen.InsertCharacters(params.Get(0, 1));
      break;
    case 'A':
      screen.CursorUp(params.Get(0, 1));
      break;
    case 'B':
    case 'e':
      screen.CursorDown(params.Get(0, 1));
      break;
    case 'C':
    case 'a':
      screen.CursorForward(params.Get(0, 1));
      break;
    case 'D':
      screen.CursorBackward(params.Get(0, 1));
      break;
    case 'E':
      screen.CursorDown(params.Get(0, 1), true);
      break;
    case 'F':
      screen.CursorUp(params.Get(0, 1), true);
      break;
    case 'G':
    case '`':
      screen.CursorToColumn(params.Get(0, 1) - 1);
      break;
    case 'H':
    case 'f':
      screen.CursorToPosition(params.Get(0, 1) - 1, params.Get(1, 1) - 1);
      break;
    case 'I':
      screen.Tab(params.Get(0, 1));
      break;
    case 'J':
      screen.EraseInDisplay(params.Get(0, 0));
      break;
    case 'K':
      screen.EraseInLine(params.Get(0, 0));
      break;
    case 'L':
      screen.InsertLines(params.Get(0, 1));
      break;
    case 'M':
      screen.DeleteLines(params.Get(0, 1));
      break;
    case 'P':
      screen.DeleteCharacters(params.Get(0, 1));
      break;
    case 'S':
      screen.ScrollUp(params.Get(0, 1));
      break;
    case 'T':
      screen.ScrollDown(params.Get(0, 1));
      break;
    case 'X':
      screen.EraseCharacters(params.Get(0, 1));
      break;
    case 'Z':
      screen.BackTab(params.Get(0, 1));
      break;
    case 'b':
      screen.RepeatLast(params.Get(0, 1));
      break;
    case 'c':
      if (config_.answer_device_queries) SendResponse("\x1b[?62;22c");
      break;
    case 'd':
      screen.CursorToRow(params.Get(0, 1) - 1);
      break;
    case 'g':
      screen.ClearTabStop(params.Get(0, 0));
      break;
    case 'h':
      for (int i = 0; i < params.count(); ++i) SetMode(params.Get(i, 0), true);
      break;
    case 'l':
      for (int i = 0; i < params.count(); ++i) SetMode(params.Get(i, 0), false);
      break;
    case 'm':
      ApplySgr(params);
      break;
    case 'n':
      if (config_.answer_device_queries) {
        int report = params.Get(0, 0);
        if (report == 5) {
          SendResponse("\x1b[0n");
        } else if (report == 6) {
          // CPR is relative to the origin when DECOM is set.
          int row = screen.cursor_row() + 1;
          int column = screen.cursor_column() + 1;
          if (screen.origin_mode()) row -= screen.scroll_top();
          SendResponse("\x1b[" + Int64ToString(row) + ";" + Int64ToString(column) + "R");
        }
      }
      break;
    case 'p':
      if (intermediates == "!") {  // DECSTR soft reset
        screen.set_origin_mode(false);
        screen.set_autowrap(true);
        screen.set_insert_mode(false);
        screen.SetScrollRegion(0, screen.rows() - 1);
        screen.set_pen(Pen());
        cursor_visible_ = true;
        application_cursor_keys_ = false;
        application_keypad_ = false;
        ++mode_revision_;
      } else if (intermediates == "$" && config_.answer_device_queries) {  // DECRQM (ANSI)
        int mode = params.Get(0, 0);
        SendResponse("\x1b[" + Int64ToString(mode) + ";" + Int64ToString(AnsiModeState(mode)) +
                     "$y");
      }
      break;
    case 'q':
      if (intermediates == " ") SetCursorStyle(params.Get(0, 0));
      break;
    case 'r':
      screen.SetScrollRegion(params.Get(0, 1) - 1, params.Get(1, screen.rows()) - 1);
      break;
    case 's':
      screen.SaveCursor();
      break;
    case 't':
      // Window manipulation: reporting sizes and moving windows are device-resource
      // operations and are ignored (spec §8.1, §12).
      break;
    case 'u':
      screen.RestoreCursor();
      break;
    default:
      // Unknown final byte: ignored, parser stays usable (spec §8.1).
      break;
  }
}

void Emulator::SetMode(int mode, bool enabled) {
  switch (mode) {
    case 4:  // IRM
      active().set_insert_mode(enabled);
      break;
    case 20:  // LNM
      newline_mode_ = enabled;
      ++mode_revision_;
      break;
    default:
      break;
  }
}

int Emulator::AnsiModeState(int mode) const {
  // DECRQM: 0 not recognised, 1 set, 2 reset.
  switch (mode) {
    case 4: return active().insert_mode() ? 1 : 2;
    case 20: return newline_mode_ ? 1 : 2;
    default: return 0;
  }
}

void Emulator::SetPrivateMode(int mode, bool enabled) {
  Screen& screen = active();
  switch (mode) {
    case 1:
      application_cursor_keys_ = enabled;
      break;
    case 3:
      // DECCOLM would resize the terminal, but the publishing device owns the PTY
      // dimensions (relay spec §6.3). The screen is cleared, the size is not changed.
      screen.EraseInDisplay(2);
      screen.CursorToPosition(0, 0);
      break;
    case 5:
      screen.set_reverse_video(enabled);
      break;
    case 6:
      screen.set_origin_mode(enabled);
      break;
    case 7:
      screen.set_autowrap(enabled);
      break;
    case 9:
      mouse_tracking_ = enabled ? MouseTracking::kX10 : MouseTracking::kOff;
      break;
    case 12:
      cursor_blinking_ = enabled;
      break;
    case 25:
      cursor_visible_ = enabled;
      break;
    case 47:
      if (enabled) {
        EnterAltScreen(false, false);
      } else {
        LeaveAltScreen(false, false);
      }
      break;
    case 66:
      application_keypad_ = enabled;
      break;
    case 1000:
      mouse_tracking_ = enabled ? MouseTracking::kNormal : MouseTracking::kOff;
      break;
    case 1002:
      mouse_tracking_ = enabled ? MouseTracking::kButtonEvent : MouseTracking::kOff;
      break;
    case 1003:
      mouse_tracking_ = enabled ? MouseTracking::kAnyEvent : MouseTracking::kOff;
      break;
    case 1004:
      focus_reporting_ = enabled;
      break;
    case 1005:
      mouse_encoding_ = enabled ? MouseEncoding::kUtf8 : MouseEncoding::kX10;
      break;
    case 1006:
      mouse_encoding_ = enabled ? MouseEncoding::kSgr : MouseEncoding::kX10;
      break;
    case 1015:
      mouse_encoding_ = enabled ? MouseEncoding::kUrxvt : MouseEncoding::kX10;
      break;
    case 1047:
      if (enabled) {
        EnterAltScreen(false, false);
      } else {
        LeaveAltScreen(false, true);
      }
      break;
    case 1048:
      if (enabled) {
        alt_saved_cursor_ = SavedCursorState();
        primary_.SaveCursor();
        alt_saved_cursor_ = primary_.saved_cursor();
      } else {
        primary_.set_saved_cursor(alt_saved_cursor_);
        primary_.RestoreCursor();
      }
      break;
    case 1049:
      if (enabled) {
        EnterAltScreen(true, true);
      } else {
        LeaveAltScreen(true, true);
      }
      break;
    case 2004:
      bracketed_paste_ = enabled;
      break;
    default:
      break;
  }
  ++mode_revision_;
}

int Emulator::PrivateModeState(int mode) const {
  switch (mode) {
    case 1: return application_cursor_keys_ ? 1 : 2;
    case 5: return active().reverse_video() ? 1 : 2;
    case 6: return active().origin_mode() ? 1 : 2;
    case 7: return active().autowrap() ? 1 : 2;
    case 12: return cursor_blinking_ ? 1 : 2;
    case 25: return cursor_visible_ ? 1 : 2;
    case 1000: return mouse_tracking_ == MouseTracking::kNormal ? 1 : 2;
    case 1002: return mouse_tracking_ == MouseTracking::kButtonEvent ? 1 : 2;
    case 1003: return mouse_tracking_ == MouseTracking::kAnyEvent ? 1 : 2;
    case 1004: return focus_reporting_ ? 1 : 2;
    case 1006: return mouse_encoding_ == MouseEncoding::kSgr ? 1 : 2;
    case 47:
    case 1047:
    case 1049: return alt_screen_active_ ? 1 : 2;
    case 2004: return bracketed_paste_ ? 1 : 2;
    default: return 0;
  }
}

void Emulator::EnterAltScreen(bool save_cursor, bool clear) {
  if (alt_screen_active_) return;
  if (save_cursor) primary_.SaveCursor();
  alt_screen_active_ = true;
  // The alternate screen inherits the current pen so an application that clears it
  // immediately gets the background it asked for.
  alt_.set_pen(primary_.pen());
  if (clear) alt_.ClearAll();
  alt_.SetScrollRegion(0, alt_.rows() - 1);
  ++mode_revision_;
}

void Emulator::LeaveAltScreen(bool restore_cursor, bool clear) {
  if (!alt_screen_active_) return;
  if (clear) alt_.ClearAll();
  alt_screen_active_ = false;
  if (restore_cursor) primary_.RestoreCursor();
  ++mode_revision_;
}

void Emulator::ApplySgr(const Params& params) {
  Screen& screen = active();
  Pen pen = screen.pen();
  if (params.count() == 0) {
    screen.set_pen(Pen());
    return;
  }

  for (int i = 0; i < params.count(); ++i) {
    std::int32_t code = params.Get(i, 0);
    switch (code) {
      case 0: pen = Pen(); break;
      case 1: pen.flags |= kFlagBold; break;
      case 2: pen.flags |= kFlagFaint; break;
      case 3: pen.flags |= kFlagItalic; break;
      case 4: {
        // SGR 4:n selects the underline style (4:0 none, 4:3 curly, ...).
        std::int32_t style = params.GetSub(i, 1, 1);
        pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagAnyUnderline);
        if (style == 0) break;
        if (style == 2) {
          pen.flags |= kFlagDoubleUnderline;
        } else if (style == 3) {
          pen.flags |= kFlagCurlyUnderline;
        } else {
          pen.flags |= kFlagUnderline;
        }
        break;
      }
      case 5: pen.flags |= kFlagBlink; break;
      case 6: pen.flags |= kFlagRapidBlink; break;
      case 7: pen.flags |= kFlagInverse; break;
      case 8: pen.flags |= kFlagConceal; break;
      case 9: pen.flags |= kFlagStrike; break;
      case 21:
        pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagAnyUnderline);
        pen.flags |= kFlagDoubleUnderline;
        break;
      case 22:
        pen.flags = static_cast<std::uint16_t>(pen.flags & ~(kFlagBold | kFlagFaint));
        break;
      case 23: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagItalic); break;
      case 24: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagAnyUnderline); break;
      case 25:
        pen.flags = static_cast<std::uint16_t>(pen.flags & ~(kFlagBlink | kFlagRapidBlink));
        break;
      case 27: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagInverse); break;
      case 28: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagConceal); break;
      case 29: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagStrike); break;
      case 39: pen.fg = Color::Default(); break;
      case 49: pen.bg = Color::Default(); break;
      case 53: pen.flags |= kFlagOverline; break;
      case 55: pen.flags = static_cast<std::uint16_t>(pen.flags & ~kFlagOverline); break;
      case 59: pen.underline_color = Color::Default(); break;
      default: {
        if (code >= 30 && code <= 37) {
          pen.fg = Color::Indexed(static_cast<std::uint8_t>(code - 30));
        } else if (code >= 40 && code <= 47) {
          pen.bg = Color::Indexed(static_cast<std::uint8_t>(code - 40));
        } else if (code >= 90 && code <= 97) {
          pen.fg = Color::Indexed(static_cast<std::uint8_t>(code - 90 + 8));
        } else if (code >= 100 && code <= 107) {
          pen.bg = Color::Indexed(static_cast<std::uint8_t>(code - 100 + 8));
        } else if (code == 38 || code == 48 || code == 58) {
          Color parsed = Color::Default();
          bool ok = false;
          if (params.SubCount(i) > 1) {
            // Colon form: 38:2:cs:r:g:b or 38:5:index — the sub-parameters belong to
            // this parameter, so no other parameter is consumed.
            std::int32_t kind = params.GetSub(i, 1, -1);
            if (kind == 5) {
              std::int32_t index = params.GetSub(i, 2, -1);
              if (index >= 0 && index <= 255) {
                parsed = Color::Indexed(static_cast<std::uint8_t>(index));
                ok = true;
              }
            } else if (kind == 2) {
              // With a colour-space id there are five sub-parameters, without it four.
              int base = params.SubCount(i) >= 6 ? 3 : 2;
              std::int32_t r = params.GetSub(i, base, -1);
              std::int32_t g = params.GetSub(i, base + 1, -1);
              std::int32_t b = params.GetSub(i, base + 2, -1);
              if (r >= 0 && r <= 255 && g >= 0 && g <= 255 && b >= 0 && b <= 255) {
                parsed = Color::Rgb(static_cast<std::uint8_t>(r), static_cast<std::uint8_t>(g),
                                    static_cast<std::uint8_t>(b));
                ok = true;
              }
            }
          } else {
            // Semicolon form: the following parameters are consumed.
            std::int32_t kind = params.Get(i + 1, -1);
            if (kind == 5 && i + 2 < params.count()) {
              std::int32_t index = params.Get(i + 2, -1);
              if (index >= 0 && index <= 255) {
                parsed = Color::Indexed(static_cast<std::uint8_t>(index));
                ok = true;
              }
              i += 2;
            } else if (kind == 2 && i + 4 < params.count()) {
              std::int32_t r = params.Get(i + 2, -1);
              std::int32_t g = params.Get(i + 3, -1);
              std::int32_t b = params.Get(i + 4, -1);
              if (r >= 0 && r <= 255 && g >= 0 && g <= 255 && b >= 0 && b <= 255) {
                parsed = Color::Rgb(static_cast<std::uint8_t>(r), static_cast<std::uint8_t>(g),
                                    static_cast<std::uint8_t>(b));
                ok = true;
              }
              i += 4;
            } else {
              // Malformed: skip what we can without consuming unrelated parameters.
              if (kind >= 0) ++i;
            }
          }
          if (ok) {
            if (code == 38) {
              pen.fg = parsed;
            } else if (code == 48) {
              pen.bg = parsed;
            } else {
              pen.underline_color = parsed;
            }
          }
        }
        break;
      }
    }
  }
  screen.set_pen(pen);
}

void Emulator::OnOsc(const std::vector<std::string>& parts, bool truncated) {
  if (parts.empty()) return;
  std::uint64_t command = 0;
  if (!ParseUint64(parts[0], 100000, &command)) return;

  switch (command) {
    case 0:  // set icon name and window title
    case 2:  // set window title
    {
      if (truncated) return;
      std::string title = parts.size() > 1 ? parts[1] : std::string();
      // Titles are remote-controlled text that ends up in the UI: strip controls and
      // bound the length before it leaves the emulator.
      title = SanitizeForMessage(title, 256);
      if (title != title_) {
        title_ = title;
        ++mode_revision_;
        if (title_callback_) title_callback_(title_);
      }
      break;
    }
    case 52: {  // clipboard
      // A read request ("?") is never answered: it would let the remote exfiltrate
      // the Android clipboard (spec §8.1, §12).
      if (parts.size() < 3) return;
      const std::string& payload = parts[2];
      if (payload == "?") return;
      if (!config_.allow_clipboard_write) return;
      Bytes decoded;
      if (!Base64Decode(payload, &decoded)) return;
      if (decoded.size() > 64 * 1024) return;
      if (clipboard_callback_) clipboard_callback_(StringFromBytes(decoded));
      break;
    }
    default:
      // Palette changes, hyperlinks, colour queries, working-directory reports and
      // everything else are ignored: they either touch device resources or would
      // require answering the remote (spec §8.1).
      break;
  }
}

void Emulator::OnDcs(const Params& params, std::uint8_t final_byte, const std::string& data,
                     bool truncated) {
  // No DCS function in the supported profile is answered. The payload is bounded by
  // the parser and discarded here; the important property is that an unterminated or
  // enormous DCS cannot wedge the parser (spec §8.1, §16.2).
  (void)params;
  (void)final_byte;
  (void)data;
  (void)truncated;
}

// ------------------------------------------------------------------- snapshots

Snapshot Emulator::BuildSnapshot(std::size_t scroll_offset, const Selection& selection) const {
  const Screen& screen = active();
  Snapshot snapshot;
  snapshot.revision = revision();
  snapshot.columns = screen.columns();
  snapshot.rows = screen.rows();
  snapshot.reverse_video = screen.reverse_video();
  snapshot.alt_screen = alt_screen_active_;
  snapshot.title = title_;
  snapshot.selection = selection;

  // The alternate screen has no scrollback (spec §8.2), so it never scrolls back.
  std::size_t available = alt_screen_active_ ? 0 : scrollback_.size();
  if (scroll_offset > available) scroll_offset = available;
  snapshot.scroll_offset = scroll_offset;
  snapshot.scrollback_size = available;

  snapshot.lines.reserve(static_cast<std::size_t>(screen.rows()));
  std::size_t start = available - scroll_offset;
  for (int row = 0; row < screen.rows(); ++row) {
    std::size_t index = start + static_cast<std::size_t>(row);
    if (index < available) {
      snapshot.lines.push_back(scrollback_.at(index));
    } else {
      snapshot.lines.push_back(screen.line_ref(static_cast<int>(index - available)));
    }
  }

  snapshot.cursor.visible = cursor_visible_;
  snapshot.cursor.blinking = cursor_blinking_;
  snapshot.cursor.shape = cursor_shape_;
  snapshot.cursor.column = screen.cursor_column();
  int cursor_row = screen.cursor_row() + static_cast<int>(scroll_offset);
  snapshot.cursor.row = (cursor_row >= 0 && cursor_row < screen.rows()) ? cursor_row : -1;
  return snapshot;
}

// --------------------------------------------------------------------- resize

void Emulator::Resize(int columns, int rows) {
  if (columns < 1) columns = 1;
  if (rows < 1) rows = 1;
  if (columns == primary_.columns() && rows == primary_.rows()) return;

  // Alternate-screen applications redraw themselves and rely on grid semantics, so
  // that buffer is never reflowed (spec §8.2).
  alt_.Resize(columns, rows, false);
  ReflowResize(columns, rows);
  ++mode_revision_;
}

void Emulator::ReflowResize(int columns, int rows) {
  const int old_columns = primary_.columns();
  const int old_rows = primary_.rows();
  if (columns == old_columns) {
    // Only the row count changed, so nothing needs rewrapping. Growing pulls lines
    // back from the scrollback rather than padding with blanks: a keyboard that opens
    // and closes, or a rotation, must not leave the screen empty where its content
    // was (spec §8.2 — resizing preserves content).
    if (rows > old_rows && !scrollback_.empty()) {
      std::vector<LineRef> restored =
          scrollback_.TakeNewest(static_cast<std::size_t>(rows - old_rows));
      if (!restored.empty()) {
        Cell blank;
        std::vector<std::shared_ptr<Line>> lines;
        lines.reserve(static_cast<std::size_t>(rows));
        for (const LineRef& line : restored) {
          // Scrollback lines are stored trimmed, so they are padded back to width.
          auto copy = std::make_shared<Line>(*line);
          copy->Resize(static_cast<std::size_t>(columns), blank);
          lines.push_back(std::move(copy));
        }
        for (const std::shared_ptr<Line>& line : primary_.lines()) lines.push_back(line);
        while (lines.size() < static_cast<std::size_t>(rows)) {
          lines.push_back(std::make_shared<Line>(static_cast<std::size_t>(columns), blank));
        }
        const int cursor_row = primary_.cursor_row() + static_cast<int>(restored.size());
        const int cursor_column = primary_.cursor_column();
        primary_.SetGeometry(columns, rows);
        primary_.ReplaceLines(std::move(lines), cursor_row, cursor_column);
        return;
      }
    }
    primary_.Resize(columns, rows, false);
    return;
  }

  // 1. Flatten the scrollback and the primary screen into logical lines.
  std::vector<LogicalLine> logical;
  std::size_t cursor_logical = 0;
  std::size_t cursor_offset = 0;
  bool cursor_placed = false;

  auto append_line = [&](const Line& line, bool continues, std::size_t used) {
    if (!continues || logical.empty()) logical.emplace_back();
    LogicalLine& target = logical.back();
    std::size_t base = target.cells.size();
    for (std::size_t column = 0; column < used && column < line.size(); ++column) {
      target.cells.push_back(line.at(column));
      const std::u32string* marks = line.Marks(column);
      if (marks != nullptr && !marks->empty()) {
        target.marks.emplace_back(base + column, *marks);
      }
    }
    return base;
  };

  bool continues = false;
  for (std::size_t i = 0; i < scrollback_.size(); ++i) {
    const Line& line = *scrollback_.at(i);
    std::size_t used = line.wrapped() ? line.size() : line.TrimmedLength();
    append_line(line, continues, used);
    continues = line.wrapped();
  }

  // Trailing blank screen rows below the cursor are dropped and re-created as blanks
  // after rewrapping, so a resize does not accumulate empty lines.
  int last_meaningful = primary_.cursor_row();
  for (int row = primary_.rows() - 1; row > last_meaningful; --row) {
    if (primary_.line(row).TrimmedLength() != 0 || primary_.line(row).wrapped()) {
      last_meaningful = row;
      break;
    }
  }

  for (int row = 0; row <= last_meaningful; ++row) {
    const Line& line = primary_.line(row);
    std::size_t used = line.wrapped() ? line.size() : line.TrimmedLength();
    if (row == primary_.cursor_row()) {
      std::size_t column = static_cast<std::size_t>(primary_.cursor_column());
      if (used < column) used = column;
    }
    std::size_t base = append_line(line, continues, used);
    if (row == primary_.cursor_row()) {
      cursor_logical = logical.empty() ? 0 : logical.size() - 1;
      cursor_offset = base + static_cast<std::size_t>(primary_.cursor_column());
      cursor_placed = true;
    }
    continues = line.wrapped();
  }
  if (logical.empty()) logical.emplace_back();
  if (!cursor_placed) {
    cursor_logical = logical.size() - 1;
    cursor_offset = 0;
  }

  // 2. Rewrap every logical line to the new width.
  Cell blank;
  std::vector<std::shared_ptr<Line>> wrapped;
  int cursor_row = 0;
  int cursor_column = 0;
  const std::size_t width = static_cast<std::size_t>(columns);
  for (std::size_t index = 0; index < logical.size(); ++index) {
    LogicalLine& source = logical[index];
    std::size_t produced = 0;
    std::size_t position = 0;
    do {
      std::size_t chunk = std::min(width, source.cells.size() - position);
      auto line = std::make_shared<Line>(width, blank);
      for (std::size_t column = 0; column < chunk; ++column) {
        line->at(column) = source.cells[position + column];
      }
      for (const auto& mark : source.marks) {
        if (mark.first >= position && mark.first < position + chunk) {
          for (char32_t c : mark.second) line->AddMark(mark.first - position, c);
        }
      }
      bool more = position + chunk < source.cells.size();
      line->set_wrapped(more);
      if (index == cursor_logical && cursor_offset >= position &&
          (cursor_offset < position + width || !more)) {
        cursor_row = static_cast<int>(wrapped.size());
        cursor_column =
            static_cast<int>(std::min(cursor_offset - position, width - 1));
      }
      wrapped.push_back(std::move(line));
      position += chunk;
      ++produced;
      // Bounded: a single logical line cannot expand without limit.
      if (produced > 1u + scrollback_.limits().max_lines) break;
    } while (position < source.cells.size());
  }

  // 3. The last `rows` wrapped lines are the screen; the rest is scrollback.
  std::vector<LineRef> new_scrollback;
  std::vector<std::shared_ptr<Line>> new_screen;
  std::size_t screen_start = wrapped.size() > static_cast<std::size_t>(rows)
                                 ? wrapped.size() - static_cast<std::size_t>(rows)
                                 : 0;
  for (std::size_t i = 0; i < screen_start; ++i) new_scrollback.push_back(wrapped[i]);
  for (std::size_t i = screen_start; i < wrapped.size(); ++i) new_screen.push_back(wrapped[i]);
  while (new_screen.size() < static_cast<std::size_t>(rows)) {
    new_screen.push_back(std::make_shared<Line>(width, blank));
  }

  scrollback_.ReplaceAll(std::move(new_scrollback));
  int screen_cursor_row = cursor_row - static_cast<int>(screen_start);
  if (screen_cursor_row < 0) screen_cursor_row = 0;
  if (screen_cursor_row >= rows) screen_cursor_row = rows - 1;

  primary_.SetGeometry(columns, rows);
  primary_.ReplaceLines(std::move(new_screen), screen_cursor_row, cursor_column);
}

}  // namespace term
}  // namespace tmirror
