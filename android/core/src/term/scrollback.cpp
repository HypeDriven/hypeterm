#include "tm/term/scrollback.h"

#include <memory>

namespace tmirror {
namespace term {

void Scrollback::SetLimits(const Limits& limits) {
  limits_ = limits;
  if (limits_.max_lines == 0) limits_.max_lines = 1;
  if (limits_.max_bytes == 0) limits_.max_bytes = 1024 * 1024;
  Trim();
}

void Scrollback::PushLine(LineRef line) {
  if (!line) return;

  // Store trailing blanks only when they carry styling; a full-width line of spaces
  // costs as much as a full line of text otherwise.
  std::size_t trimmed = line->TrimmedLength();
  if (trimmed < line->size()) {
    auto copy = std::make_shared<Line>(*line);
    Cell blank;
    copy->Resize(trimmed, blank);
    line = copy;
  }

  bytes_ += line->MemoryBytes();
  lines_.push_back(std::move(line));
  ++pushed_;
  ++revision_;
  Trim();
}

void Scrollback::ClearScrollback() {
  if (lines_.empty()) return;
  evicted_ += lines_.size();
  lines_.clear();
  bytes_ = 0;
  ++revision_;
}

void Scrollback::ReplaceAll(std::vector<LineRef> lines) {
  lines_.clear();
  bytes_ = 0;
  for (auto& line : lines) {
    if (!line) continue;
    bytes_ += line->MemoryBytes();
    lines_.push_back(std::move(line));
  }
  ++revision_;
  Trim();
}

std::vector<LineRef> Scrollback::TakeNewest(std::size_t count) {
  if (count > lines_.size()) count = lines_.size();
  std::vector<LineRef> taken;
  taken.reserve(count);
  for (std::size_t i = lines_.size() - count; i < lines_.size(); ++i) {
    bytes_ -= lines_[i]->MemoryBytes();
    taken.push_back(lines_[i]);
  }
  lines_.erase(lines_.end() - static_cast<std::ptrdiff_t>(count), lines_.end());
  if (count > 0) ++revision_;
  return taken;
}

void Scrollback::Trim() {
  while (lines_.size() > limits_.max_lines ||
         (bytes_ > limits_.max_bytes && lines_.size() > 1)) {
    bytes_ -= lines_.front()->MemoryBytes();
    lines_.pop_front();
    ++evicted_;
    ++revision_;
  }
}

}  // namespace term
}  // namespace tmirror
