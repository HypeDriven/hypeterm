#include "tm/app/session.h"

#include <algorithm>

#include "tm/term/utf8.h"

namespace tmirror {
namespace app {
namespace {

term::EmulatorConfig MakeEmulatorConfig(const AppConfig& config) {
  term::EmulatorConfig emulator;
  emulator.columns = config.fallback_columns;
  emulator.rows = config.fallback_rows;
  emulator.scrollback = config.scrollback;
  emulator.parser = config.parser;
  emulator.allow_clipboard_write = config.allow_clipboard_write;
  emulator.answer_device_queries = config.answer_device_queries;
  return emulator;
}

void AppendCell(const term::Line& line, std::size_t column, std::string* out) {
  const term::Cell& cell = line.at(column);
  if (cell.is_continuation()) return;
  term::AppendUtf8(cell.code == 0 ? U' ' : cell.code, out);
  const std::u32string* marks = line.Marks(column);
  if (marks != nullptr) {
    for (char32_t mark : *marks) term::AppendUtf8(mark, out);
  }
}

std::string LineText(const term::Line& line, std::size_t from, std::size_t to) {
  std::string text;
  std::size_t end = std::min(to, line.size());
  for (std::size_t column = from; column < end; ++column) {
    AppendCell(line, column, &text);
  }
  // Trailing blanks are not part of a copied line.
  while (!text.empty() && text.back() == ' ') text.pop_back();
  return text;
}

}  // namespace

TerminalSession::TerminalSession(const AppConfig& config)
    : config_(config), emulator_(MakeEmulatorConfig(config)) {}

void TerminalSession::ApplyOutput(ByteView bytes) {
  if (bytes.empty()) return;
  const std::uint64_t pushed_before = emulator_.scrollback().pushed_lines();
  emulator_.Feed(bytes);

  // New output while the user is scrolled back must not yank the viewport away, but
  // the offset counts *from the live bottom*: every line that moved into the scrollback
  // pushes the region the user is reading one line further back. Without this the view
  // drifts downwards as output arrives (spec §5.2).
  //
  // Counted from lines *pushed*, not from the change in retained size: once the ring is
  // full it evicts as fast as it fills, so its size stops growing while the bottom keeps
  // moving. Measuring the size would report no movement at exactly the moment there is
  // most of it, and the view would slide a line per line of output — the drift this is
  // here to prevent, only during a long build rather than a short one.
  if (scroll_offset_ > 0) {
    const std::uint64_t pushed =
        emulator_.scrollback().pushed_lines() - pushed_before;
    scroll_offset_ += static_cast<std::size_t>(pushed);
    const std::size_t retained = emulator_.max_scroll_offset();
    if (scroll_offset_ > retained) scroll_offset_ = retained;
  }
}

void TerminalSession::ResetTerminal() {
  emulator_.Reset();
  scroll_offset_ = 0;
  selection_ = term::Selection();
}

void TerminalSession::ResizeGrid(int columns, int rows) {
  if (columns < 1 || rows < 1) return;
  emulator_.Resize(columns, rows);
  std::size_t maximum = emulator_.max_scroll_offset();
  if (scroll_offset_ > maximum) scroll_offset_ = maximum;
  selection_ = term::Selection();
}

void TerminalSession::ScrollLines(int delta) {
  // The alternate screen keeps no scrollback (spec §8.2), so there is nothing here to
  // scroll to: the snapshot clamps the offset back to zero and the screen does not
  // move. Without this guard the offset still counts up against the *primary* screen's
  // history, which silently parks the session away from the live bottom — a full-screen
  // application would stop being followed, and the screen would offer a way back to an
  // output the user never left.
  if (emulator_.alt_screen_active()) return;
  std::size_t maximum = emulator_.max_scroll_offset();
  if (delta > 0) {
    std::size_t increase = static_cast<std::size_t>(delta);
    scroll_offset_ = std::min(maximum, scroll_offset_ + increase);
  } else if (delta < 0) {
    std::size_t decrease = static_cast<std::size_t>(-delta);
    scroll_offset_ = decrease >= scroll_offset_ ? 0 : scroll_offset_ - decrease;
  }
}

void TerminalSession::ScrollToBottom() { scroll_offset_ = 0; }

term::SnapshotRef TerminalSession::PublishSnapshot() {
  term::Snapshot built = emulator_.BuildSnapshot(scroll_offset_, selection_);
  // The emulator clamps the offset it was handed to the scrollback it actually kept,
  // so the snapshot's own offset cannot answer this on the alternate screen. Only the
  // session knows where the user parked the view.
  built.following_output = following_output();
  auto snapshot = std::make_shared<const term::Snapshot>(std::move(built));
  published_revision_ = snapshot->revision;
  {
    std::lock_guard<std::mutex> lock(snapshot_mutex_);
    latest_ = snapshot;
  }
  return snapshot;
}

term::SnapshotRef TerminalSession::latest_snapshot() const {
  std::lock_guard<std::mutex> lock(snapshot_mutex_);
  return latest_;
}

bool TerminalSession::NeedsPublish() const {
  return emulator_.revision() != published_revision_;
}

input::KeyboardModes TerminalSession::keyboard_modes() const {
  input::KeyboardModes modes;
  modes.application_cursor = emulator_.application_cursor_keys();
  modes.application_keypad = emulator_.application_keypad();
  modes.newline_mode = emulator_.newline_mode();
  modes.bracketed_paste = emulator_.bracketed_paste();
  return modes;
}

std::string ExtractSelection(const term::Snapshot& snapshot) {
  const term::Selection& selection = snapshot.selection;
  if (!selection.active) return std::string();

  int start_row = selection.start_row;
  int start_column = selection.start_column;
  int end_row = selection.end_row;
  int end_column = selection.end_column;
  if (start_row > end_row || (start_row == end_row && start_column > end_column)) {
    std::swap(start_row, end_row);
    std::swap(start_column, end_column);
  }
  start_row = std::max(0, start_row);
  end_row = std::min(end_row, snapshot.rows - 1);

  std::string text;
  for (int row = start_row; row <= end_row; ++row) {
    const term::Line* line = snapshot.line(row);
    if (line == nullptr) continue;
    std::size_t from = 0;
    std::size_t to = line->size();
    if (selection.rectangular) {
      from = static_cast<std::size_t>(std::max(0, std::min(start_column, end_column)));
      to = static_cast<std::size_t>(std::max(start_column, end_column)) + 1;
    } else {
      if (row == start_row) from = static_cast<std::size_t>(std::max(0, start_column));
      if (row == end_row) to = static_cast<std::size_t>(std::max(0, end_column)) + 1;
    }
    text += LineText(*line, from, to);
    if (row != end_row) {
      // A wrapped line is one logical line: no newline is inserted at the wrap.
      if (selection.rectangular || !line->wrapped()) text.push_back('\n');
    }
  }
  return text;
}

std::string ExtractVisibleText(const term::Snapshot& snapshot) {
  std::string text;
  for (int row = 0; row < snapshot.rows; ++row) {
    const term::Line* line = snapshot.line(row);
    if (line == nullptr) continue;
    text += LineText(*line, 0, line->size());
    if (row + 1 < snapshot.rows) text.push_back('\n');
  }
  return text;
}

}  // namespace app
}  // namespace tmirror
