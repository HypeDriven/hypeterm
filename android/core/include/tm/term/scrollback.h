#pragma once

#include <cstddef>
#include <deque>

#include "tm/term/screen.h"

namespace tmirror {
namespace term {

/// Bounded scrollback for the primary screen (spec §8.2).
///
/// Two independent bounds apply, because either one alone can be defeated: a line
/// count (default 10,000 logical lines) and a memory ceiling, since 10,000 lines of a
/// very wide terminal is a different amount of memory from 10,000 lines of a narrow
/// one. Lines are stored trimmed of trailing blanks.
class Scrollback : public ScrollbackSink {
 public:
  struct Limits {
    std::size_t max_lines = 10000;
    std::size_t max_bytes = 32u * 1024u * 1024u;
  };

  Scrollback() = default;
  explicit Scrollback(const Limits& limits) : limits_(limits) {}

  void SetLimits(const Limits& limits);
  const Limits& limits() const { return limits_; }

  void PushLine(LineRef line) override;
  void ClearScrollback() override;

  /// Replace the whole buffer, used by the reflow path when a resize redistributes
  /// lines between the screen and the scrollback.
  void ReplaceAll(std::vector<LineRef> lines);

  /// Removes and returns the newest `count` lines, oldest first. Used when the screen
  /// grows and those lines belong back on it.
  std::vector<LineRef> TakeNewest(std::size_t count);

  std::size_t size() const { return lines_.size(); }
  bool empty() const { return lines_.empty(); }
  /// Index 0 is the oldest retained line.
  LineRef at(std::size_t index) const { return lines_[index]; }
  std::size_t memory_bytes() const { return bytes_; }
  std::uint64_t revision() const { return revision_; }

  /// Number of lines discarded since construction, for diagnostics.
  std::uint64_t evicted_lines() const { return evicted_; }

  /// Number of lines that have ever scrolled in here, whether they were kept or not.
  ///
  /// The one honest measure of how far the live bottom has moved. `size()` stops
  /// growing once the ring is full and eviction keeps pace with arrival, and it drops
  /// outright when the scrollback is cleared, so neither it nor `evicted_lines()` alone
  /// can tell a reader parked in the history how far the text under them has travelled.
  std::uint64_t pushed_lines() const { return pushed_; }

 private:
  void Trim();

  Limits limits_;
  std::deque<LineRef> lines_;
  std::size_t bytes_ = 0;
  std::uint64_t evicted_ = 0;
  std::uint64_t pushed_ = 0;
  std::uint64_t revision_ = 1;
};

}  // namespace term
}  // namespace tmirror
