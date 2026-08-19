#include "tm/term/screen.h"

#include <algorithm>
#include <utility>

namespace tmirror {
namespace term {

// ---------------------------------------------------------------------- Line

void Line::Resize(std::size_t columns, const Cell& blank) {
  if (columns < cells_.size()) {
    // Dropping a leading wide cell would leave its continuation orphaned, and vice
    // versa; normalise both halves before truncating.
    if (columns > 0 && cells_[columns - 1].width == 2) {
      cells_[columns - 1].code = U' ';
      cells_[columns - 1].width = 1;
      cells_[columns - 1].flags =
          static_cast<std::uint16_t>(cells_[columns - 1].flags & ~kFlagHasMarks);
    }
    ClearMarksInRange(columns, cells_.size());
  }
  cells_.resize(columns, blank);
}

void Line::Fill(const Cell& blank) {
  std::fill(cells_.begin(), cells_.end(), blank);
  marks_.clear();
  wrapped_ = false;
}

void Line::FillRange(std::size_t begin, std::size_t end, const Cell& blank) {
  if (begin >= cells_.size()) return;
  if (end > cells_.size()) end = cells_.size();
  if (begin >= end) return;
  std::fill(cells_.begin() + static_cast<std::ptrdiff_t>(begin),
            cells_.begin() + static_cast<std::ptrdiff_t>(end), blank);
  ClearMarksInRange(begin, end);
}

const std::u32string* Line::Marks(std::size_t column) const {
  for (const auto& entry : marks_) {
    if (entry.first == column) return &entry.second;
  }
  return nullptr;
}

void Line::AddMark(std::size_t column, char32_t mark) {
  if (column >= cells_.size()) return;
  for (auto& entry : marks_) {
    if (entry.first == column) {
      if (entry.second.size() < kMaxMarksPerCell) entry.second.push_back(mark);
      return;
    }
  }
  // Bounded: a hostile stream of combining marks must not grow this without limit.
  if (marks_.size() >= cells_.size()) return;
  marks_.emplace_back(static_cast<std::uint16_t>(column), std::u32string(1, mark));
  cells_[column].flags |= kFlagHasMarks;
}

void Line::ClearMarks(std::size_t column) {
  for (std::size_t i = 0; i < marks_.size(); ++i) {
    if (marks_[i].first == column) {
      marks_.erase(marks_.begin() + static_cast<std::ptrdiff_t>(i));
      break;
    }
  }
  if (column < cells_.size()) {
    cells_[column].flags = static_cast<std::uint16_t>(cells_[column].flags & ~kFlagHasMarks);
  }
}

void Line::ClearMarksInRange(std::size_t begin, std::size_t end) {
  marks_.erase(std::remove_if(marks_.begin(), marks_.end(),
                              [&](const std::pair<std::uint16_t, std::u32string>& entry) {
                                return entry.first >= begin && entry.first < end;
                              }),
               marks_.end());
  for (std::size_t i = begin; i < end && i < cells_.size(); ++i) {
    cells_[i].flags = static_cast<std::uint16_t>(cells_[i].flags & ~kFlagHasMarks);
  }
}

void Line::ShiftMarks(std::size_t from, std::ptrdiff_t delta, std::size_t limit) {
  std::vector<std::pair<std::uint16_t, std::u32string>> kept;
  kept.reserve(marks_.size());
  for (auto& entry : marks_) {
    if (entry.first < from) {
      kept.push_back(std::move(entry));
      continue;
    }
    std::ptrdiff_t moved = static_cast<std::ptrdiff_t>(entry.first) + delta;
    if (moved < 0 || moved >= static_cast<std::ptrdiff_t>(limit)) continue;
    entry.first = static_cast<std::uint16_t>(moved);
    kept.push_back(std::move(entry));
  }
  marks_ = std::move(kept);
}

std::size_t Line::TrimmedLength() const {
  std::size_t length = cells_.size();
  while (length > 0) {
    const Cell& cell = cells_[length - 1];
    bool blank = (cell.code == U' ' || cell.code == 0) && cell.bg.is_default() &&
                 (cell.flags & ~kFlagHasMarks) == 0 && !cell.has_marks();
    if (!blank) break;
    --length;
  }
  return length;
}

std::size_t Line::MemoryBytes() const {
  std::size_t bytes = sizeof(Line) + cells_.capacity() * sizeof(Cell);
  for (const auto& entry : marks_) {
    bytes += sizeof(entry) + entry.second.capacity() * sizeof(char32_t);
  }
  return bytes;
}

// -------------------------------------------------------------------- Screen

namespace {
int Clamp(int value, int low, int high) {
  if (value < low) return low;
  if (value > high) return high;
  return value;
}
}  // namespace

Screen::Screen(int columns, int rows, ScrollbackSink* scrollback)
    : columns_(columns < 1 ? 1 : columns),
      rows_(rows < 1 ? 1 : rows),
      scrollback_(scrollback) {
  lines_.reserve(static_cast<std::size_t>(rows_));
  Cell blank = BlankCell(pen_);
  for (int i = 0; i < rows_; ++i) {
    lines_.push_back(std::make_shared<Line>(static_cast<std::size_t>(columns_), blank));
  }
  scroll_top_ = 0;
  scroll_bottom_ = rows_ - 1;
  ResetTabStops();
}

Line& Screen::MutableLine(int row) {
  auto& ref = lines_[static_cast<std::size_t>(row)];
  // Copy on write: a snapshot or the scrollback may still be holding this line.
  if (ref.use_count() > 1) ref = std::make_shared<Line>(*ref);
  ++revision_;
  return *ref;
}

void Screen::set_origin_mode(bool value) {
  origin_mode_ = value;
  // DECOM homes the cursor to the (possibly relative) origin.
  cursor_row_ = TopLimit();
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::set_reverse_video(bool value) {
  if (reverse_video_ != value) ++revision_;
  reverse_video_ = value;
}

void Screen::SetScrollRegion(int top, int bottom) {
  if (top < 0) top = 0;
  if (bottom < 0 || bottom > rows_ - 1) bottom = rows_ - 1;
  if (top >= bottom) {
    // An inverted or degenerate region resets to the full screen (DEC behaviour).
    top = 0;
    bottom = rows_ - 1;
  }
  scroll_top_ = top;
  scroll_bottom_ = bottom;
  cursor_row_ = TopLimit();
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::ClampCursor() {
  cursor_row_ = Clamp(cursor_row_, 0, rows_ - 1);
  cursor_column_ = Clamp(cursor_column_, 0, columns_ - 1);
}

void Screen::ClearWideCharAt(Line& line, int column) {
  if (column < 0 || column >= columns_) return;
  Cell& cell = line.at(static_cast<std::size_t>(column));
  if (cell.is_continuation() && column > 0) {
    Cell& lead = line.at(static_cast<std::size_t>(column - 1));
    if (lead.width == 2) {
      lead.code = U' ';
      lead.width = 1;
      line.ClearMarks(static_cast<std::size_t>(column - 1));
    }
    cell.code = U' ';
    cell.width = 1;
  } else if (cell.width == 2 && column + 1 < columns_) {
    Cell& tail = line.at(static_cast<std::size_t>(column + 1));
    if (tail.is_continuation()) {
      tail.code = U' ';
      tail.width = 1;
    }
    line.ClearMarks(static_cast<std::size_t>(column));
  }
}

void Screen::PutChar(char32_t code_point, int width) {
  if (width <= 0) {
    AddCombiningMark(code_point);
    return;
  }

  if (pending_wrap_) {
    MutableLine(cursor_row_).set_wrapped(true);
    cursor_column_ = 0;
    LineFeed();
    pending_wrap_ = false;
  }

  if (width == 2 && cursor_column_ == columns_ - 1) {
    if (autowrap_) {
      // A double-width character never straddles the right margin.
      Line& current = MutableLine(cursor_row_);
      current.set_wrapped(true);
      current.FillRange(static_cast<std::size_t>(cursor_column_),
                        static_cast<std::size_t>(cursor_column_ + 1), BlankCell(pen_));
      cursor_column_ = 0;
      LineFeed();
    } else {
      // No room and no wrapping: the cell is blanked rather than half-drawn.
      Line& current = MutableLine(cursor_row_);
      ClearWideCharAt(current, cursor_column_);
      current.FillRange(static_cast<std::size_t>(cursor_column_),
                        static_cast<std::size_t>(cursor_column_ + 1), BlankCell(pen_));
      return;
    }
  }

  // IRM shifts the rest of the line right before the write. Done before taking the
  // line reference below, because InsertCharacters may copy the line.
  if (insert_mode_) InsertCharacters(width);

  Line& line = MutableLine(cursor_row_);
  ClearWideCharAt(line, cursor_column_);
  if (width == 2) ClearWideCharAt(line, cursor_column_ + 1);

  Cell& cell = line.at(static_cast<std::size_t>(cursor_column_));
  line.ClearMarks(static_cast<std::size_t>(cursor_column_));
  cell.code = code_point;
  cell.fg = pen_.fg;
  cell.bg = pen_.bg;
  cell.underline_color = pen_.underline_color;
  cell.flags = pen_.flags;
  cell.width = static_cast<std::uint8_t>(width);

  last_written_row_ = cursor_row_;
  last_written_column_ = cursor_column_;
  last_written_char_ = code_point;
  last_written_width_ = width;

  if (width == 2 && cursor_column_ + 1 < columns_) {
    Cell& tail = line.at(static_cast<std::size_t>(cursor_column_ + 1));
    line.ClearMarks(static_cast<std::size_t>(cursor_column_ + 1));
    tail = cell;
    tail.code = 0;  // continuation marker
    tail.width = 0;
    tail.flags = static_cast<std::uint16_t>(tail.flags & ~kFlagHasMarks);
  }

  cursor_column_ += width;
  if (cursor_column_ >= columns_) {
    cursor_column_ = columns_ - 1;
    pending_wrap_ = autowrap_;
  }
}

void Screen::AddCombiningMark(char32_t mark) {
  if (last_written_row_ < 0 || last_written_row_ >= rows_) return;
  if (last_written_column_ < 0 || last_written_column_ >= columns_) return;
  Line& line = MutableLine(last_written_row_);
  line.AddMark(static_cast<std::size_t>(last_written_column_), mark);
}

void Screen::RepeatLast(int count) {
  if (last_written_char_ == 0 || count <= 0) return;
  // Bounded: REP with a huge parameter must not become unbounded work.
  count = std::min(count, columns_ * rows_);
  char32_t code_point = last_written_char_;
  int width = last_written_width_;
  for (int i = 0; i < count; ++i) PutChar(code_point, width);
}

void Screen::CarriageReturn() {
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::LineFeed() {
  if (cursor_row_ == scroll_bottom_) {
    ScrollRegionUp(scroll_top_, scroll_bottom_, 1, scroll_top_ == 0);
  } else if (cursor_row_ < rows_ - 1) {
    ++cursor_row_;
  }
  pending_wrap_ = false;
}

void Screen::ReverseIndex() {
  if (cursor_row_ == scroll_top_) {
    ScrollRegionDown(scroll_top_, scroll_bottom_, 1);
  } else if (cursor_row_ > 0) {
    --cursor_row_;
  }
  pending_wrap_ = false;
}

void Screen::Backspace() {
  if (pending_wrap_) {
    pending_wrap_ = false;
    return;
  }
  if (cursor_column_ > 0) --cursor_column_;
}

void Screen::Tab(int count) {
  for (int i = 0; i < count && cursor_column_ < columns_ - 1; ++i) {
    int column = cursor_column_ + 1;
    while (column < columns_ - 1 && !tab_stops_[static_cast<std::size_t>(column)]) ++column;
    cursor_column_ = column;
  }
  pending_wrap_ = false;
}

void Screen::BackTab(int count) {
  for (int i = 0; i < count && cursor_column_ > 0; ++i) {
    int column = cursor_column_ - 1;
    while (column > 0 && !tab_stops_[static_cast<std::size_t>(column)]) --column;
    cursor_column_ = column;
  }
  pending_wrap_ = false;
}

void Screen::SetTabStop() {
  if (cursor_column_ >= 0 && cursor_column_ < columns_) {
    tab_stops_[static_cast<std::size_t>(cursor_column_)] = true;
  }
}

void Screen::ClearTabStop(int mode) {
  if (mode == 3) {
    std::fill(tab_stops_.begin(), tab_stops_.end(), false);
  } else if (cursor_column_ >= 0 && cursor_column_ < columns_) {
    tab_stops_[static_cast<std::size_t>(cursor_column_)] = false;
  }
}

void Screen::ResetTabStops() {
  tab_stops_.assign(static_cast<std::size_t>(columns_), false);
  for (int column = 8; column < columns_; column += 8) {
    tab_stops_[static_cast<std::size_t>(column)] = true;
  }
}

void Screen::CursorUp(int count, bool reset_column) {
  if (count < 1) count = 1;
  int limit = (cursor_row_ >= scroll_top_) ? scroll_top_ : 0;
  cursor_row_ = std::max(limit, cursor_row_ - count);
  if (reset_column) cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::CursorDown(int count, bool reset_column) {
  if (count < 1) count = 1;
  int limit = (cursor_row_ <= scroll_bottom_) ? scroll_bottom_ : rows_ - 1;
  cursor_row_ = std::min(limit, cursor_row_ + count);
  if (reset_column) cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::CursorForward(int count) {
  if (count < 1) count = 1;
  cursor_column_ = std::min(columns_ - 1, cursor_column_ + count);
  pending_wrap_ = false;
}

void Screen::CursorBackward(int count) {
  if (count < 1) count = 1;
  cursor_column_ = std::max(0, cursor_column_ - count);
  pending_wrap_ = false;
}

void Screen::CursorToColumn(int column) {
  cursor_column_ = Clamp(column, 0, columns_ - 1);
  pending_wrap_ = false;
}

void Screen::CursorToRow(int row) {
  int top = TopLimit();
  cursor_row_ = Clamp(row + top, top, BottomLimit());
  pending_wrap_ = false;
}

void Screen::CursorToPosition(int row, int column) {
  int top = TopLimit();
  cursor_row_ = Clamp(row + top, top, BottomLimit());
  cursor_column_ = Clamp(column, 0, columns_ - 1);
  pending_wrap_ = false;
}

void Screen::SaveCursor() {
  saved_.row = cursor_row_;
  saved_.column = cursor_column_;
  saved_.pen = pen_;
  saved_.origin_mode = origin_mode_;
  saved_.autowrap = autowrap_;
  saved_.valid = true;
}

void Screen::RestoreCursor() {
  if (!saved_.valid) {
    cursor_row_ = 0;
    cursor_column_ = 0;
    pending_wrap_ = false;
    return;
  }
  origin_mode_ = saved_.origin_mode;
  autowrap_ = saved_.autowrap;
  pen_ = saved_.pen;
  cursor_row_ = Clamp(saved_.row, 0, rows_ - 1);
  cursor_column_ = Clamp(saved_.column, 0, columns_ - 1);
  pending_wrap_ = false;
}

void Screen::EraseInDisplay(int mode) {
  Cell blank = BlankCell(pen_);
  switch (mode) {
    case 0:
      MutableLine(cursor_row_).FillRange(static_cast<std::size_t>(cursor_column_),
                                         static_cast<std::size_t>(columns_), blank);
      MutableLine(cursor_row_).set_wrapped(false);
      for (int row = cursor_row_ + 1; row < rows_; ++row) {
        MutableLine(row).Fill(blank);
      }
      break;
    case 1:
      MutableLine(cursor_row_).FillRange(0, static_cast<std::size_t>(cursor_column_) + 1, blank);
      for (int row = 0; row < cursor_row_; ++row) {
        MutableLine(row).Fill(blank);
      }
      break;
    case 2:
      for (int row = 0; row < rows_; ++row) MutableLine(row).Fill(blank);
      break;
    case 3:
      if (scrollback_ != nullptr) scrollback_->ClearScrollback();
      break;
    default:
      break;
  }
  pending_wrap_ = false;
}

void Screen::EraseInLine(int mode) {
  Cell blank = BlankCell(pen_);
  Line& line = MutableLine(cursor_row_);
  switch (mode) {
    case 0:
      line.FillRange(static_cast<std::size_t>(cursor_column_), static_cast<std::size_t>(columns_),
                     blank);
      line.set_wrapped(false);
      break;
    case 1:
      line.FillRange(0, static_cast<std::size_t>(cursor_column_) + 1, blank);
      break;
    case 2:
      line.Fill(blank);
      break;
    default:
      break;
  }
  pending_wrap_ = false;
}

void Screen::EraseCharacters(int count) {
  if (count < 1) count = 1;
  Cell blank = BlankCell(pen_);
  Line& line = MutableLine(cursor_row_);
  std::size_t end = static_cast<std::size_t>(cursor_column_) + static_cast<std::size_t>(count);
  ClearWideCharAt(line, cursor_column_);
  line.FillRange(static_cast<std::size_t>(cursor_column_), end, blank);
  pending_wrap_ = false;
}

void Screen::InsertCharacters(int count) {
  if (count < 1) count = 1;
  if (count > columns_ - cursor_column_) count = columns_ - cursor_column_;
  if (count <= 0) return;
  Line& line = MutableLine(cursor_row_);
  ClearWideCharAt(line, cursor_column_);
  for (int column = columns_ - 1; column >= cursor_column_ + count; --column) {
    line.at(static_cast<std::size_t>(column)) =
        line.at(static_cast<std::size_t>(column - count));
  }
  line.ShiftMarks(static_cast<std::size_t>(cursor_column_), count,
                  static_cast<std::size_t>(columns_));
  line.FillRange(static_cast<std::size_t>(cursor_column_),
                 static_cast<std::size_t>(cursor_column_ + count), BlankCell(pen_));
  pending_wrap_ = false;
}

void Screen::DeleteCharacters(int count) {
  if (count < 1) count = 1;
  if (count > columns_ - cursor_column_) count = columns_ - cursor_column_;
  if (count <= 0) return;
  Line& line = MutableLine(cursor_row_);
  ClearWideCharAt(line, cursor_column_);
  for (int column = cursor_column_; column < columns_ - count; ++column) {
    line.at(static_cast<std::size_t>(column)) =
        line.at(static_cast<std::size_t>(column + count));
  }
  line.ShiftMarks(static_cast<std::size_t>(cursor_column_), -count,
                  static_cast<std::size_t>(columns_));
  line.FillRange(static_cast<std::size_t>(columns_ - count), static_cast<std::size_t>(columns_),
                 BlankCell(pen_));
  pending_wrap_ = false;
}

void Screen::InsertLines(int count) {
  if (cursor_row_ < scroll_top_ || cursor_row_ > scroll_bottom_) return;
  if (count < 1) count = 1;
  ScrollRegionDown(cursor_row_, scroll_bottom_, count);
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::DeleteLines(int count) {
  if (cursor_row_ < scroll_top_ || cursor_row_ > scroll_bottom_) return;
  if (count < 1) count = 1;
  // Deleting lines never fills scrollback: only content scrolling off the top of the
  // screen does (spec §8.2).
  ScrollRegionUp(cursor_row_, scroll_bottom_, count, false);
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::ScrollUp(int count) {
  if (count < 1) count = 1;
  ScrollRegionUp(scroll_top_, scroll_bottom_, count, scroll_top_ == 0);
}

void Screen::ScrollDown(int count) {
  if (count < 1) count = 1;
  ScrollRegionDown(scroll_top_, scroll_bottom_, count);
}

void Screen::ScrollRegionUp(int top, int bottom, int count, bool to_scrollback) {
  if (top < 0 || bottom >= rows_ || top > bottom) return;
  int height = bottom - top + 1;
  if (count > height) count = height;
  if (count <= 0) return;

  Cell blank = BlankCell(pen_);
  for (int i = 0; i < count; ++i) {
    if (to_scrollback && scrollback_ != nullptr) {
      scrollback_->PushLine(lines_[static_cast<std::size_t>(top + i)]);
    }
  }
  for (int row = top; row + count <= bottom; ++row) {
    lines_[static_cast<std::size_t>(row)] = lines_[static_cast<std::size_t>(row + count)];
  }
  for (int row = bottom - count + 1; row <= bottom; ++row) {
    // A fresh line, never a recycled one: the old object may still be referenced by
    // the scrollback or by a snapshot the renderer is holding.
    lines_[static_cast<std::size_t>(row)] =
        std::make_shared<Line>(static_cast<std::size_t>(columns_), blank);
  }
  ++revision_;
}

void Screen::ScrollRegionDown(int top, int bottom, int count) {
  if (top < 0 || bottom >= rows_ || top > bottom) return;
  int height = bottom - top + 1;
  if (count > height) count = height;
  if (count <= 0) return;

  Cell blank = BlankCell(pen_);
  for (int row = bottom; row - count >= top; --row) {
    lines_[static_cast<std::size_t>(row)] = lines_[static_cast<std::size_t>(row - count)];
  }
  for (int row = top; row < top + count; ++row) {
    lines_[static_cast<std::size_t>(row)] =
        std::make_shared<Line>(static_cast<std::size_t>(columns_), blank);
  }
  ++revision_;
}

void Screen::FillScreen(char32_t code_point) {
  Cell fill = BlankCell(pen_);
  fill.code = code_point;
  for (int row = 0; row < rows_; ++row) MutableLine(row).Fill(fill);
  cursor_row_ = 0;
  cursor_column_ = 0;
  pending_wrap_ = false;
}

void Screen::ClearAll() {
  Cell blank = BlankCell(pen_);
  for (int row = 0; row < rows_; ++row) {
    lines_[static_cast<std::size_t>(row)] =
        std::make_shared<Line>(static_cast<std::size_t>(columns_), blank);
  }
  cursor_row_ = 0;
  cursor_column_ = 0;
  pending_wrap_ = false;
  last_written_row_ = -1;
  last_written_column_ = -1;
  ++revision_;
}

void Screen::Reset() {
  pen_ = Pen();
  origin_mode_ = false;
  autowrap_ = true;
  insert_mode_ = false;
  reverse_video_ = false;
  scroll_top_ = 0;
  scroll_bottom_ = rows_ - 1;
  saved_ = SavedCursorState();
  last_written_char_ = 0;
  last_written_width_ = 1;
  ResetTabStops();
  ClearAll();
}

void Screen::Resize(int columns, int rows, bool reflow) {
  (void)reflow;  // reflow is driven by the emulator, which also owns the scrollback
  columns = columns < 1 ? 1 : columns;
  rows = rows < 1 ? 1 : rows;
  if (columns == columns_ && rows == rows_) return;

  Cell blank = BlankCell(Pen());
  if (columns != columns_) {
    for (auto& line : lines_) {
      // Shared lines must be cloned before a width change: the scrollback keeps its
      // own width and a snapshot in flight must not change under the renderer.
      if (line.use_count() > 1) line = std::make_shared<Line>(*line);
      line->Resize(static_cast<std::size_t>(columns), blank);
    }
    columns_ = columns;
  }

  if (rows != rows_) {
    if (rows < rows_) {
      // Prefer discarding blank lines below the cursor; otherwise scroll the top
      // away, which is what a shell user expects when the keyboard appears.
      int excess = rows_ - rows;
      int removable_below = 0;
      for (int row = rows_ - 1; row > cursor_row_ && removable_below < excess; --row) {
        if (lines_[static_cast<std::size_t>(row)]->TrimmedLength() != 0) break;
        ++removable_below;
      }
      int drop_below = std::min(excess, removable_below);
      for (int i = 0; i < drop_below; ++i) lines_.pop_back();
      int drop_top = excess - drop_below;
      for (int i = 0; i < drop_top; ++i) {
        if (scrollback_ != nullptr) scrollback_->PushLine(lines_.front());
        lines_.erase(lines_.begin());
        if (cursor_row_ > 0) --cursor_row_;
      }
    } else {
      for (int i = rows_; i < rows; ++i) {
        lines_.push_back(std::make_shared<Line>(static_cast<std::size_t>(columns_), blank));
      }
    }
    rows_ = rows;
  }

  scroll_top_ = 0;
  scroll_bottom_ = rows_ - 1;
  ResetTabStops();
  ClampCursor();
  pending_wrap_ = false;
  last_written_row_ = -1;
  last_written_column_ = -1;
  ++revision_;
}

void Screen::SetGeometry(int columns, int rows) {
  columns_ = columns < 1 ? 1 : columns;
  rows_ = rows < 1 ? 1 : rows;
  scroll_top_ = 0;
  scroll_bottom_ = rows_ - 1;
  ResetTabStops();
  pending_wrap_ = false;
  last_written_row_ = -1;
  last_written_column_ = -1;
  ++revision_;
}

void Screen::ReplaceLines(std::vector<std::shared_ptr<Line>> lines, int cursor_row,
                          int cursor_column) {
  lines_ = std::move(lines);
  rows_ = static_cast<int>(lines_.size());
  if (rows_ < 1) {
    lines_.push_back(std::make_shared<Line>(static_cast<std::size_t>(columns_), BlankCell(Pen())));
    rows_ = 1;
  }
  scroll_top_ = 0;
  scroll_bottom_ = rows_ - 1;
  cursor_row_ = Clamp(cursor_row, 0, rows_ - 1);
  cursor_column_ = Clamp(cursor_column, 0, columns_ - 1);
  pending_wrap_ = false;
  last_written_row_ = -1;
  last_written_column_ = -1;
  ++revision_;
}

}  // namespace term
}  // namespace tmirror
