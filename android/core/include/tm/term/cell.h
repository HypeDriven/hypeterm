#pragma once

#include <cstdint>
#include <string>
#include <utility>
#include <vector>

namespace tmirror {
namespace term {

/// A terminal colour: the default (theme-defined) colour, one of the 256 palette
/// indices, or a 24-bit direct colour (spec §8.1).
class Color {
 public:
  enum class Kind : std::uint8_t { kDefault = 0, kIndexed = 1, kRgb = 2 };

  constexpr Color() = default;

  static constexpr Color Default() { return Color(); }
  static constexpr Color Indexed(std::uint8_t index) {
    return Color(static_cast<std::uint32_t>(Kind::kIndexed) << 24 | index);
  }
  static constexpr Color Rgb(std::uint8_t r, std::uint8_t g, std::uint8_t b) {
    return Color(static_cast<std::uint32_t>(Kind::kRgb) << 24 |
                 static_cast<std::uint32_t>(r) << 16 | static_cast<std::uint32_t>(g) << 8 | b);
  }

  Kind kind() const { return static_cast<Kind>(value_ >> 24); }
  bool is_default() const { return kind() == Kind::kDefault; }
  std::uint8_t index() const { return static_cast<std::uint8_t>(value_ & 0xFF); }
  std::uint8_t red() const { return static_cast<std::uint8_t>((value_ >> 16) & 0xFF); }
  std::uint8_t green() const { return static_cast<std::uint8_t>((value_ >> 8) & 0xFF); }
  std::uint8_t blue() const { return static_cast<std::uint8_t>(value_ & 0xFF); }
  std::uint32_t raw() const { return value_; }

  bool operator==(const Color& other) const { return value_ == other.value_; }
  bool operator!=(const Color& other) const { return value_ != other.value_; }

 private:
  explicit constexpr Color(std::uint32_t value) : value_(value) {}
  std::uint32_t value_ = 0;
};

/// SGR attributes (spec §8.1). Underline styles are mutually exclusive and share the
/// low bits so a renderer can switch on them.
enum CellFlags : std::uint16_t {
  kFlagNone = 0,
  kFlagBold = 1u << 0,
  kFlagFaint = 1u << 1,
  kFlagItalic = 1u << 2,
  kFlagUnderline = 1u << 3,
  kFlagDoubleUnderline = 1u << 4,
  kFlagCurlyUnderline = 1u << 5,
  kFlagBlink = 1u << 6,
  kFlagRapidBlink = 1u << 7,
  kFlagInverse = 1u << 8,
  kFlagConceal = 1u << 9,
  kFlagStrike = 1u << 10,
  kFlagOverline = 1u << 11,
  /// The cell carries combining marks, held on the owning Line.
  kFlagHasMarks = 1u << 12,
  kFlagAnyUnderline = kFlagUnderline | kFlagDoubleUnderline | kFlagCurlyUnderline,
};

/// Drawing state applied to newly written cells.
struct Pen {
  Color fg;
  Color bg;
  /// Colour of the underline when set separately (SGR 58); default follows fg.
  Color underline_color;
  std::uint16_t flags = kFlagNone;

  bool operator==(const Pen& other) const {
    return fg == other.fg && bg == other.bg && underline_color == other.underline_color &&
           flags == other.flags;
  }
  bool operator!=(const Pen& other) const { return !(*this == other); }
};

/// One grid cell.
///
/// `code == 0` marks the continuation half of a double-width character: it holds the
/// same colours as its leading cell so background fills stay contiguous, and the
/// renderer skips it (spec §8.2 "continuation marker").
struct Cell {
  char32_t code = U' ';
  Color fg;
  Color bg;
  Color underline_color;
  std::uint16_t flags = kFlagNone;
  std::uint8_t width = 1;

  bool is_continuation() const { return code == 0; }
  bool has_marks() const { return (flags & kFlagHasMarks) != 0; }

  bool SameStyle(const Cell& other) const {
    return fg == other.fg && bg == other.bg && underline_color == other.underline_color &&
           flags == other.flags;
  }
};

inline Cell BlankCell(const Pen& pen) {
  Cell cell;
  cell.code = U' ';
  // Erasing keeps the background colour but never the text attributes, matching
  // xterm: an erase with a coloured background paints, an erase under `bold` does
  // not leave bold blanks behind.
  cell.fg = pen.fg;
  cell.bg = pen.bg;
  cell.underline_color = pen.underline_color;
  cell.flags = static_cast<std::uint16_t>(pen.flags & (kFlagInverse | kFlagBlink));
  cell.width = 1;
  return cell;
}

/// One row of cells plus the combining marks that belong to it.
///
/// Marks live on the line rather than in the cell so an ordinary Cell stays 16 bytes;
/// scrollback holds millions of them and almost none carry marks.
class Line {
 public:
  static constexpr std::size_t kMaxMarksPerCell = 8;

  Line() = default;
  explicit Line(std::size_t columns, const Cell& blank = Cell()) : cells_(columns, blank) {}

  std::size_t size() const { return cells_.size(); }
  bool empty() const { return cells_.empty(); }
  const Cell& at(std::size_t column) const { return cells_[column]; }
  Cell& at(std::size_t column) { return cells_[column]; }
  const std::vector<Cell>& cells() const { return cells_; }

  bool wrapped() const { return wrapped_; }
  void set_wrapped(bool wrapped) { wrapped_ = wrapped; }

  void Resize(std::size_t columns, const Cell& blank);
  void Fill(const Cell& blank);
  void FillRange(std::size_t begin, std::size_t end, const Cell& blank);

  /// Combining marks for a column, or nullptr.
  const std::u32string* Marks(std::size_t column) const;
  void AddMark(std::size_t column, char32_t mark);
  void ClearMarks(std::size_t column);
  void ClearMarksInRange(std::size_t begin, std::size_t end);
  void ClearAllMarks() { marks_.clear(); }
  void ShiftMarks(std::size_t from, std::ptrdiff_t delta, std::size_t limit);
  const std::vector<std::pair<std::uint16_t, std::u32string>>& marks() const { return marks_; }

  /// Column after the last non-blank cell; used to trim scrollback and to reflow.
  std::size_t TrimmedLength() const;

  /// Approximate heap cost, for the scrollback memory ceiling (spec §8.2).
  std::size_t MemoryBytes() const;

 private:
  std::vector<Cell> cells_;
  std::vector<std::pair<std::uint16_t, std::u32string>> marks_;
  bool wrapped_ = false;
};

}  // namespace term
}  // namespace tmirror
