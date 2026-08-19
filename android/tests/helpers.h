#pragma once

#include <string>

#include "framework.h"
#include "tm/term/emulator.h"
#include "tm/term/utf8.h"

namespace tmtest {

/// Text of one row of a screen, trailing blanks trimmed.
inline std::string RowText(const tmirror::term::Screen& screen, int row) {
  std::string text;
  const tmirror::term::Line& line = screen.line(row);
  for (std::size_t column = 0; column < line.size(); ++column) {
    const tmirror::term::Cell& cell = line.at(column);
    if (cell.is_continuation()) continue;
    tmirror::term::AppendUtf8(cell.code == 0 ? U' ' : cell.code, &text);
    const std::u32string* marks = line.Marks(column);
    if (marks != nullptr) {
      for (char32_t mark : *marks) tmirror::term::AppendUtf8(mark, &text);
    }
  }
  while (!text.empty() && text.back() == ' ') text.pop_back();
  return text;
}

inline std::string RowText(const tmirror::term::Emulator& emulator, int row) {
  return RowText(emulator.active(), row);
}

/// All visible rows joined with newlines, trailing blank rows removed.
inline std::string ScreenText(const tmirror::term::Emulator& emulator) {
  std::string text;
  for (int row = 0; row < emulator.active().rows(); ++row) {
    if (row != 0) text.push_back('\n');
    text += RowText(emulator, row);
  }
  while (!text.empty() && (text.back() == '\n')) text.pop_back();
  return text;
}

/// Feeds bytes in fixed-size chunks, which is how the emulator sees a real stream.
inline void FeedInChunks(tmirror::term::Emulator& emulator, const std::string& bytes,
                         std::size_t chunk_size) {
  if (chunk_size == 0) chunk_size = 1;
  for (std::size_t offset = 0; offset < bytes.size(); offset += chunk_size) {
    std::size_t length = std::min(chunk_size, bytes.size() - offset);
    emulator.Feed(tmirror::ByteView::FromChars(bytes.data() + offset, length));
  }
}

inline void Feed(tmirror::term::Emulator& emulator, const std::string& bytes) {
  emulator.Feed(tmirror::ByteView(bytes));
}

inline tmirror::term::EmulatorConfig SmallConfig(int columns = 20, int rows = 5) {
  tmirror::term::EmulatorConfig config;
  config.columns = columns;
  config.rows = rows;
  config.scrollback.max_lines = 100;
  return config;
}

}  // namespace tmtest
