#pragma once

#include <memory>
#include <mutex>
#include <string>

#include "tm/app/config.h"
#include "tm/input/key_encoder.h"
#include "tm/term/emulator.h"
#include "tm/term/snapshot.h"

namespace tmirror {
namespace app {

/// One attached terminal: the emulator, the view state, and the snapshot handed to
/// the renderer.
///
/// Everything except `latest_snapshot()` is owned by the network/parser thread. The
/// renderer only ever sees an immutable snapshot, which is what lets parsing continue
/// while a frame is being drawn (spec §6.2).
class TerminalSession {
 public:
  explicit TerminalSession(const AppConfig& config);

  term::Emulator& emulator() { return emulator_; }
  const term::Emulator& emulator() const { return emulator_; }

  /// Apply terminal output bytes. Parsing happens here, never on the render or UI
  /// thread.
  void ApplyOutput(ByteView bytes);

  /// Full reset, used after an unrecoverable gap (relay spec §6.2).
  void ResetTerminal();

  /// Adopt the authoritative grid size reported by the relay.
  void ResizeGrid(int columns, int rows);

  // ------------------------------------------------------------- view state
  void ScrollLines(int delta);
  void ScrollToBottom();
  std::size_t scroll_offset() const { return scroll_offset_; }
  bool following_output() const { return scroll_offset_ == 0; }
  void SetSelection(const term::Selection& selection) { selection_ = selection; }
  void ClearSelection() { selection_ = term::Selection(); }
  const term::Selection& selection() const { return selection_; }

  // -------------------------------------------------------------- snapshots
  /// Rebuilds the snapshot and stores it for the renderer. Returns the new snapshot.
  term::SnapshotRef PublishSnapshot();
  /// Thread-safe read of the most recently published snapshot.
  term::SnapshotRef latest_snapshot() const;
  /// True when the terminal has changed since the last publish.
  bool NeedsPublish() const;

  input::KeyboardModes keyboard_modes() const;

  std::uint64_t published_revision() const { return published_revision_; }

 private:
  AppConfig config_;
  term::Emulator emulator_;
  std::size_t scroll_offset_ = 0;
  term::Selection selection_;
  std::uint64_t published_revision_ = 0;

  mutable std::mutex snapshot_mutex_;
  term::SnapshotRef latest_;
};

/// Extracts the selected text from a snapshot, joining wrapped lines and trimming
/// trailing blanks the way a terminal copy is expected to.
std::string ExtractSelection(const term::Snapshot& snapshot);

/// Extracts all visible text, used by the accessibility bridge (spec §13).
std::string ExtractVisibleText(const term::Snapshot& snapshot);

}  // namespace app
}  // namespace tmirror
