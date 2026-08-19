#pragma once

#include <cstdint>
#include <memory>
#include <string>
#include <vector>

#include "tm/term/screen.h"

namespace tmirror {
namespace term {

enum class CursorShape { kBlock, kUnderline, kBar };

struct CursorState {
  /// Row within the snapshot's viewport, or -1 when the cursor is scrolled out of
  /// view (the renderer then draws no cursor).
  int row = 0;
  int column = 0;
  bool visible = true;
  bool blinking = true;
  CursorShape shape = CursorShape::kBlock;
};

/// A text selection in viewport coordinates.
struct Selection {
  bool active = false;
  int start_row = 0;
  int start_column = 0;
  int end_row = 0;
  int end_column = 0;  // inclusive
  bool rectangular = false;
};

/// An immutable view of the terminal handed to the render thread (spec §6.2).
///
/// Lines are shared pointers into the emulator's copy-on-write storage, so building
/// one costs a pointer copy per visible row and the renderer can hold it for as long
/// as it likes without blocking the parser.
struct Snapshot {
  std::uint64_t revision = 0;
  int columns = 0;
  int rows = 0;
  std::vector<LineRef> lines;
  CursorState cursor;
  Selection selection;
  bool reverse_video = false;
  bool alt_screen = false;
  /// Lines scrolled up from the live bottom; 0 means "following the output".
  std::size_t scroll_offset = 0;
  std::size_t scrollback_size = 0;
  /// Whether the session is parked at the live bottom, watching output arrive.
  ///
  /// Deliberately not the same question as `scroll_offset == 0`. The alternate screen
  /// has no scrollback (spec §8.2), so the offset above is clamped to zero there while
  /// the user is still scrolled back in the primary screen; reading the clamped value
  /// would report somebody who ran `less` from a scrolled-back prompt as having
  /// returned to the bottom.
  bool following_output = true;
  std::string title;

  bool empty() const { return lines.empty(); }
  /// Line for a viewport row, or nullptr when out of range.
  const Line* line(int row) const {
    if (row < 0 || static_cast<std::size_t>(row) >= lines.size()) return nullptr;
    return lines[static_cast<std::size_t>(row)].get();
  }
};

using SnapshotRef = std::shared_ptr<const Snapshot>;

}  // namespace term
}  // namespace tmirror
