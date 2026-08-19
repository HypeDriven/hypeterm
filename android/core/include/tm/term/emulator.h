#pragma once

#include <functional>
#include <string>

#include "tm/term/parser.h"
#include "tm/term/screen.h"
#include "tm/term/scrollback.h"
#include "tm/term/snapshot.h"

namespace tmirror {
namespace term {

enum class MouseTracking { kOff, kX10, kNormal, kButtonEvent, kAnyEvent };
enum class MouseEncoding { kX10, kUtf8, kSgr, kUrxvt };

struct EmulatorConfig {
  int columns = 80;
  int rows = 24;
  Scrollback::Limits scrollback;
  Parser::Limits parser;
  /// OSC 52 writes are a device-resource operation and are ignored unless a
  /// separately reviewed policy enables them (spec §8.1, §12). OSC 52 *reads* are
  /// never honoured, regardless of this flag.
  bool allow_clipboard_write = false;
  /// Answer DA/DSR/DECRQM queries. On by default: full-screen applications rely on
  /// them. Replies travel as terminal input and are dropped when the session has no
  /// input authority.
  bool answer_device_queries = true;
};

/// The terminal state machine (spec §8).
///
/// It owns the primary and alternate screens, the scrollback, the parser and every
/// mode that changes how bytes are interpreted. It never touches the network, never
/// allocates without a bound, and treats every byte it is given as hostile.
class Emulator : public ParserHandler {
 public:
  using ResponseSink = std::function<void(ByteView)>;
  using TitleCallback = std::function<void(const std::string& title)>;
  using BellCallback = std::function<void()>;
  using ClipboardCallback = std::function<void(const std::string& utf8_text)>;

  explicit Emulator(const EmulatorConfig& config = EmulatorConfig());

  /// Apply terminal output bytes. Never throws, never blocks.
  void Feed(ByteView bytes);

  /// Full reset. Used for RIS and after an unrecoverable stream gap, where the
  /// screen contents are no longer reconstructible (relay spec §6.2).
  void Reset();

  /// Adopt a new grid size. The relay's publisher owns the real PTY dimensions, so
  /// this is normally driven by a `terminal.resize` message; the local request path
  /// only *asks* for a size (relay spec §6.3).
  void Resize(int columns, int rows);

  int columns() const { return active().columns(); }
  int rows() const { return active().rows(); }

  const Screen& active() const { return alt_screen_active_ ? alt_ : primary_; }
  Screen& active() { return alt_screen_active_ ? alt_ : primary_; }
  const Screen& primary() const { return primary_; }
  const Scrollback& scrollback() const { return scrollback_; }
  Scrollback& scrollback() { return scrollback_; }
  bool alt_screen_active() const { return alt_screen_active_; }

  /// Monotonic counter over every observable change, used to decide whether a redraw
  /// is needed at all (spec §10.1: no continuous rendering at idle).
  std::uint64_t revision() const;

  /// Build a viewport snapshot. `scroll_offset` counts lines scrolled up from the
  /// live bottom and is clamped to the retained scrollback.
  Snapshot BuildSnapshot(std::size_t scroll_offset = 0,
                         const Selection& selection = Selection()) const;

  // ------------------------------------------------------------------- modes
  bool application_cursor_keys() const { return application_cursor_keys_; }
  bool application_keypad() const { return application_keypad_; }
  bool bracketed_paste() const { return bracketed_paste_; }
  bool focus_reporting() const { return focus_reporting_; }
  bool newline_mode() const { return newline_mode_; }
  bool cursor_visible() const { return cursor_visible_; }
  bool cursor_blinking() const { return cursor_blinking_; }
  CursorShape cursor_shape() const { return cursor_shape_; }
  MouseTracking mouse_tracking() const { return mouse_tracking_; }
  MouseEncoding mouse_encoding() const { return mouse_encoding_; }
  const std::string& title() const { return title_; }
  std::size_t max_scroll_offset() const { return scrollback_.size(); }

  // --------------------------------------------------------------- callbacks
  void SetResponseSink(ResponseSink sink) { response_sink_ = std::move(sink); }
  void SetTitleCallback(TitleCallback callback) { title_callback_ = std::move(callback); }
  void SetBellCallback(BellCallback callback) { bell_callback_ = std::move(callback); }
  void SetClipboardCallback(ClipboardCallback callback) {
    clipboard_callback_ = std::move(callback);
  }

  /// Emit a report to the remote terminal (focus events, mouse reports, replies).
  void SendResponse(const std::string& bytes);

  // ParserHandler
  void OnPrint(char32_t code_point) override;
  void OnExecute(std::uint8_t control) override;
  void OnEscape(const std::string& intermediates, std::uint8_t final_byte) override;
  void OnCsi(const Params& params, std::uint8_t final_byte) override;
  void OnOsc(const std::vector<std::string>& parts, bool truncated) override;
  void OnDcs(const Params& params, std::uint8_t final_byte, const std::string& data,
             bool truncated) override;

 private:
  void SetMode(int mode, bool enabled);
  void SetPrivateMode(int mode, bool enabled);
  int PrivateModeState(int mode) const;
  int AnsiModeState(int mode) const;
  void ApplySgr(const Params& params);
  void EnterAltScreen(bool save_cursor, bool clear);
  void LeaveAltScreen(bool restore_cursor, bool clear);
  void ReflowResize(int columns, int rows);
  char32_t TranslateCharset(char32_t code_point) const;
  void HandleDecSequence(const std::string& intermediates, std::uint8_t final_byte);
  void SetCursorStyle(int style);

  EmulatorConfig config_;
  Scrollback scrollback_;
  Screen primary_;
  Screen alt_;
  Parser parser_;
  bool alt_screen_active_ = false;

  // Modes that are not grid mechanics live here rather than on the Screen.
  bool application_cursor_keys_ = false;
  bool application_keypad_ = false;
  bool bracketed_paste_ = false;
  bool focus_reporting_ = false;
  bool newline_mode_ = false;
  bool cursor_visible_ = true;
  bool cursor_blinking_ = true;
  CursorShape cursor_shape_ = CursorShape::kBlock;
  MouseTracking mouse_tracking_ = MouseTracking::kOff;
  MouseEncoding mouse_encoding_ = MouseEncoding::kX10;

  // Character sets: G0/G1 with SI/SO (needed for box drawing in vim, tmux, dialog).
  int charset_g0_ = 'B';
  int charset_g1_ = 'B';
  int active_charset_ = 0;

  SavedCursorState alt_saved_cursor_;
  std::string title_;
  std::uint64_t mode_revision_ = 1;

  ResponseSink response_sink_;
  TitleCallback title_callback_;
  BellCallback bell_callback_;
  ClipboardCallback clipboard_callback_;
};

}  // namespace term
}  // namespace tmirror
