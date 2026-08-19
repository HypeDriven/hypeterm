// Resize storms interleaved with output floods (spec §16.2).
//
// Reflow is the most intricate part of the emulator: it moves lines between the
// screen and the scrollback, rewraps them, and has to keep the cursor on the
// character it was on. This target hammers it with arbitrary sizes at arbitrary
// points in an arbitrary stream.

#include <cassert>
#include <cstdint>

#include "tm/term/emulator.h"

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size) {
  tmirror::term::EmulatorConfig config;
  config.columns = 20;
  config.rows = 6;
  config.scrollback.max_lines = 100;
  tmirror::term::Emulator emulator(config);

  std::size_t position = 0;
  while (position < size) {
    std::uint8_t control = data[position++];
    if ((control & 0x0F) == 0 && position + 1 < size) {
      // Resize to a size derived from the stream, including degenerate ones.
      int columns = 1 + (data[position] % 120);
      int rows = 1 + (data[position + 1] % 60);
      position += 2;
      emulator.Resize(columns, rows);
      assert(emulator.columns() == columns);
      assert(emulator.rows() == rows);
      assert(emulator.active().cursor_column() < emulator.columns());
      assert(emulator.active().cursor_row() < emulator.rows());
      for (int row = 0; row < emulator.rows(); ++row) {
        assert(static_cast<int>(emulator.active().line(row).size()) == emulator.columns());
      }
      continue;
    }
    if ((control & 0x0F) == 1) {
      // Toggle the alternate screen, whose resize policy differs deliberately.
      emulator.Feed(tmirror::ByteView(std::string(
          emulator.alt_screen_active() ? "\x1b[?1049l" : "\x1b[?1049h")));
      continue;
    }

    std::size_t chunk = 1 + (control % 32);
    if (position + chunk > size) chunk = size - position;
    if (chunk == 0) break;
    emulator.Feed(tmirror::ByteView(data + position, chunk));
    position += chunk;
  }

  assert(emulator.scrollback().size() <= config.scrollback.max_lines);
  tmirror::term::Snapshot snapshot = emulator.BuildSnapshot(emulator.max_scroll_offset());
  assert(static_cast<int>(snapshot.lines.size()) == snapshot.rows);
  return 0;
}
