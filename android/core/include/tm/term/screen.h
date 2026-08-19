#pragma once

#include <cstdint>
#include <memory>
#include <vector>

#include "tm/term/cell.h"

namespace tmirror {
namespace term {

/// Lines are reference-counted and copied on write, so producing a render snapshot
/// costs one pointer copy per visible row instead of a full grid copy (spec §6.2:
/// the renderer consumes immutable snapshots).
using LineRef = std::shared_ptr<const Line>;

/// Receives lines that scroll off the top of the primary screen.
class ScrollbackSink {
 public:
  virtual ~ScrollbackSink() = default;
  virtual void PushLine(LineRef line) = 0;
  /// CSI 3 J: drop saved lines.
  virtual void ClearScrollback() = 0;
};

struct SavedCursorState {
  int row = 0;
  int column = 0;
  Pen pen;
  bool origin_mode = false;
  bool autowrap = true;
  int charset_g0 = 0;
  int charset_g1 = 0;
  bool valid = false;
};

/// One screen buffer: the primary buffer or the alternate buffer (spec §8.2).
///
/// The Screen owns grid mechanics only. Mode flags that change *how* a sequence is
/// interpreted live here when the grid needs them (origin, autowrap, insert); the
/// rest live on the Emulator.
class Screen {
 public:
  Screen(int columns, int rows, ScrollbackSink* scrollback);

  int columns() const { return columns_; }
  int rows() const { return rows_; }
  std::uint64_t revision() const { return revision_; }
  void MarkDirty() { ++revision_; }

  const Line& line(int row) const { return *lines_[static_cast<std::size_t>(row)]; }
  LineRef line_ref(int row) const { return lines_[static_cast<std::size_t>(row)]; }
  Line& MutableLine(int row);

  int cursor_row() const { return cursor_row_; }
  int cursor_column() const { return cursor_column_; }
  bool pending_wrap() const { return pending_wrap_; }

  const Pen& pen() const { return pen_; }
  void set_pen(const Pen& pen) { pen_ = pen; }

  bool origin_mode() const { return origin_mode_; }
  void set_origin_mode(bool value);
  bool autowrap() const { return autowrap_; }
  void set_autowrap(bool value) { autowrap_ = value; }
  bool insert_mode() const { return insert_mode_; }
  void set_insert_mode(bool value) { insert_mode_ = value; }
  bool reverse_video() const { return reverse_video_; }
  void set_reverse_video(bool value);

  int scroll_top() const { return scroll_top_; }
  int scroll_bottom() const { return scroll_bottom_; }
  void SetScrollRegion(int top, int bottom);

  // ------------------------------------------------------------------ writing

  /// Write one code point of the given display width at the cursor, applying
  /// deferred wrap, insert mode and wide-character placement.
  void PutChar(char32_t code_point, int width);
  /// Attach a zero-width mark to the most recently written cell.
  void AddCombiningMark(char32_t mark);
  /// ECMA-48 REP: repeat the last printed character.
  void RepeatLast(int count);

  // ---------------------------------------------------------------- movement

  void CarriageReturn();
  void LineFeed();       // IND behaviour with scrolling at the region bottom
  void ReverseIndex();   // RI
  void Backspace();
  void Tab(int count);
  void BackTab(int count);
  void SetTabStop();
  void ClearTabStop(int mode);
  void ResetTabStops();

  void CursorUp(int count, bool reset_column = false);
  void CursorDown(int count, bool reset_column = false);
  void CursorForward(int count);
  void CursorBackward(int count);
  void CursorToColumn(int column);        // 0-based, absolute
  void CursorToRow(int row);              // 0-based, respects origin mode
  void CursorToPosition(int row, int column);
  void CursorForwardTabbed(int count) { Tab(count); }

  void SaveCursor();
  void RestoreCursor();
  const SavedCursorState& saved_cursor() const { return saved_; }
  void set_saved_cursor(const SavedCursorState& state) { saved_ = state; }

  // ----------------------------------------------------------------- editing

  void EraseInDisplay(int mode);  // 0 below, 1 above, 2 all, 3 scrollback
  void EraseInLine(int mode);     // 0 right, 1 left, 2 all
  void EraseCharacters(int count);
  void InsertCharacters(int count);
  void DeleteCharacters(int count);
  void InsertLines(int count);
  void DeleteLines(int count);
  void ScrollUp(int count);
  void ScrollDown(int count);
  /// Fill the whole screen with a character (DECALN).
  void FillScreen(char32_t code_point);

  void ClearAll();
  void Reset();

  /// Resize the grid. `reflow` rewraps wrapped logical lines and is used for the
  /// primary screen; the alternate screen preserves grid semantics instead
  /// (spec §8.2). Returns the number of lines pushed to scrollback by the resize.
  void Resize(int columns, int rows, bool reflow);

  /// Used by the reflow path in the emulator, which owns both the screen and the
  /// scrollback and therefore has to move lines between them.
  const std::vector<std::shared_ptr<Line>>& lines() const { return lines_; }
  /// Adopt new dimensions without touching the grid; the caller immediately follows
  /// with ReplaceLines. Kept separate from Resize so a reflow does not also run the
  /// non-reflow row policy and push lines into the scrollback twice.
  void SetGeometry(int columns, int rows);
  void ReplaceLines(std::vector<std::shared_ptr<Line>> lines, int cursor_row,
                    int cursor_column);

  Cell BlankCellForErase() const { return BlankCell(pen_); }

 private:
  void ScrollRegionUp(int top, int bottom, int count, bool to_scrollback);
  void ScrollRegionDown(int top, int bottom, int count);
  void ClampCursor();
  int TopLimit() const { return origin_mode_ ? scroll_top_ : 0; }
  int BottomLimit() const { return origin_mode_ ? scroll_bottom_ : rows_ - 1; }
  void ClearWideCharAt(Line& line, int column);

  int columns_;
  int rows_;
  ScrollbackSink* scrollback_;
  std::vector<std::shared_ptr<Line>> lines_;

  int cursor_row_ = 0;
  int cursor_column_ = 0;
  bool pending_wrap_ = false;
  /// Column of the last written cell, for combining marks and REP.
  int last_written_row_ = -1;
  int last_written_column_ = -1;
  char32_t last_written_char_ = 0;
  int last_written_width_ = 1;

  Pen pen_;
  bool origin_mode_ = false;
  bool autowrap_ = true;
  bool insert_mode_ = false;
  bool reverse_video_ = false;
  int scroll_top_ = 0;
  int scroll_bottom_ = 0;
  std::vector<bool> tab_stops_;
  SavedCursorState saved_;
  std::uint64_t revision_ = 1;
};

}  // namespace term
}  // namespace tmirror
