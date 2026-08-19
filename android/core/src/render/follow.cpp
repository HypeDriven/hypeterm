#include "tm/render/follow.h"

namespace tmirror {
namespace render {

OutputAnchor AnchorForOutput(const term::Snapshot& snapshot, const CellMetrics& metrics) {
  OutputAnchor anchor;
  if (snapshot.empty() || snapshot.rows <= 0 || !metrics.valid()) return anchor;

  int row = -1;
  if (snapshot.cursor.visible && snapshot.cursor.row >= 0 &&
      snapshot.cursor.row < snapshot.rows) {
    // The cursor is where the next character appears, on either screen. In an editor or
    // a pager it is also where the user's attention is, so following it is the most
    // useful thing this can do on a grid larger than the phone.
    row = snapshot.cursor.row;
  } else if (!snapshot.alt_screen) {
    // A hidden caret — a spinner, a progress bar, a build log — still scrolls its output
    // onto the last row, which is a reliable anchor on a screen that scrolls.
    row = snapshot.rows - 1;
  } else {
    // A full-screen application that hid its caret gives no signal at all, and its
    // "latest output" is the whole canvas. Guessing would drag a zoomed-in reader to an
    // arbitrary corner, so nothing moves.
    return anchor;
  }

  anchor.top = static_cast<float>(row) * metrics.cell_height;
  anchor.height = metrics.cell_height;
  anchor.valid = true;
  return anchor;
}

}  // namespace render
}  // namespace tmirror
