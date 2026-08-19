#include "tm/render/metrics.h"

#include <algorithm>
#include <cmath>

namespace tmirror {
namespace render {
namespace {

Rgba MakeRgba(std::uint8_t r, std::uint8_t g, std::uint8_t b) { return Rgba{r, g, b, 255}; }

}  // namespace

Palette::Palette() {
  // The 16 ANSI colours, in the usual xterm values.
  static const Rgba kBase[16] = {
      {0x00, 0x00, 0x00, 0xFF}, {0xCD, 0x00, 0x00, 0xFF}, {0x00, 0xCD, 0x00, 0xFF},
      {0xCD, 0xCD, 0x00, 0xFF}, {0x00, 0x00, 0xEE, 0xFF}, {0xCD, 0x00, 0xCD, 0xFF},
      {0x00, 0xCD, 0xCD, 0xFF}, {0xE5, 0xE5, 0xE5, 0xFF}, {0x7F, 0x7F, 0x7F, 0xFF},
      {0xFF, 0x00, 0x00, 0xFF}, {0x00, 0xFF, 0x00, 0xFF}, {0xFF, 0xFF, 0x00, 0xFF},
      {0x5C, 0x5C, 0xFF, 0xFF}, {0xFF, 0x00, 0xFF, 0xFF}, {0x00, 0xFF, 0xFF, 0xFF},
      {0xFF, 0xFF, 0xFF, 0xFF},
  };
  for (int i = 0; i < 16; ++i) palette_[i] = kBase[i];

  // 216-colour cube.
  static const std::uint8_t kSteps[6] = {0, 95, 135, 175, 215, 255};
  int index = 16;
  for (int r = 0; r < 6; ++r) {
    for (int g = 0; g < 6; ++g) {
      for (int b = 0; b < 6; ++b) {
        palette_[index++] = MakeRgba(kSteps[r], kSteps[g], kSteps[b]);
      }
    }
  }
  // 24 greys.
  for (int i = 0; i < 24; ++i) {
    std::uint8_t level = static_cast<std::uint8_t>(8 + i * 10);
    palette_[index++] = MakeRgba(level, level, level);
  }

  default_foreground_ = MakeRgba(0xD8, 0xD8, 0xD8);
  default_background_ = MakeRgba(0x10, 0x12, 0x16);
  selection_ = Rgba{0x3A, 0x5C, 0x8C, 0xFF};
  cursor_ = MakeRgba(0xD8, 0xD8, 0xD8);
}

void Palette::SetIndexed(int index, Rgba color) {
  if (index >= 0 && index < 256) palette_[index] = color;
}

Rgba Palette::indexed(int index) const {
  if (index < 0 || index >= 256) return default_foreground_;
  return palette_[index];
}

Rgba Palette::Resolve(const term::Color& color, bool foreground) const {
  switch (color.kind()) {
    case term::Color::Kind::kDefault:
      return foreground ? default_foreground_ : default_background_;
    case term::Color::Kind::kIndexed:
      return palette_[color.index()];
    case term::Color::Kind::kRgb:
      return Rgba{color.red(), color.green(), color.blue(), 255};
  }
  return foreground ? default_foreground_ : default_background_;
}

Rgba Palette::Blend(Rgba a, Rgba b, float t) {
  if (t < 0.0f) t = 0.0f;
  if (t > 1.0f) t = 1.0f;
  auto mix = [&](std::uint8_t x, std::uint8_t y) {
    float value = static_cast<float>(x) * (1.0f - t) + static_cast<float>(y) * t;
    return static_cast<std::uint8_t>(std::lround(std::max(0.0f, std::min(255.0f, value))));
  };
  return Rgba{mix(a.r, b.r), mix(a.g, b.g), mix(a.b, b.b), mix(a.a, b.a)};
}

void Palette::ResolvePair(const term::Cell& cell, bool reverse_video, Rgba* foreground,
                          Rgba* background) const {
  Rgba fg = Resolve(cell.fg, true);
  Rgba bg = Resolve(cell.bg, false);

  // Bold with a palette colour brightens it, as xterm does, but never a direct colour.
  if ((cell.flags & term::kFlagBold) != 0 && cell.fg.kind() == term::Color::Kind::kIndexed &&
      cell.fg.index() < 8) {
    fg = palette_[cell.fg.index() + 8];
  }
  if ((cell.flags & term::kFlagFaint) != 0) fg = Blend(fg, bg, 0.5f);

  bool inverse = (cell.flags & term::kFlagInverse) != 0;
  if (reverse_video) inverse = !inverse;
  if (inverse) std::swap(fg, bg);

  // Concealed text keeps its cell background so the layout does not shift.
  if ((cell.flags & term::kFlagConceal) != 0) {
    *foreground = bg;
    *background = bg;
    return;
  }

  if (minimum_contrast_ > 1.0f) fg = EnforceContrast(fg, bg);

  *foreground = fg;
  *background = bg;
}

float Palette::RelativeLuminance(Rgba color) {
  auto channel = [](std::uint8_t value) {
    float v = static_cast<float>(value) / 255.0f;
    return v <= 0.03928f ? v / 12.92f : std::pow((v + 0.055f) / 1.055f, 2.4f);
  };
  return 0.2126f * channel(color.r) + 0.7152f * channel(color.g) + 0.0722f * channel(color.b);
}

float Palette::ContrastRatio(Rgba a, Rgba b) {
  float first = RelativeLuminance(a);
  float second = RelativeLuminance(b);
  if (first < second) std::swap(first, second);
  return (first + 0.05f) / (second + 0.05f);
}

Rgba Palette::EnforceContrast(Rgba foreground, Rgba background) const {
  if (ContrastRatio(foreground, background) >= minimum_contrast_) return foreground;

  // Push away from the background: towards white on a dark background, towards black
  // on a light one. Sixteen steps is finer than the eye can tell and bounded.
  const Rgba target = RelativeLuminance(background) < 0.5f ? Rgba{255, 255, 255, 255}
                                                           : Rgba{0, 0, 0, 255};
  Rgba best = foreground;
  for (int step = 1; step <= 16; ++step) {
    Rgba candidate = Blend(foreground, target, static_cast<float>(step) / 16.0f);
    best = candidate;
    if (ContrastRatio(candidate, background) >= minimum_contrast_) break;
  }
  return best;
}

GridSize ComputeGrid(float width_px, float height_px, const CellMetrics& metrics,
                     float padding_px) {
  GridSize grid;
  if (!metrics.valid()) return GridSize{1, 1};
  float usable_width = width_px - 2.0f * padding_px;
  float usable_height = height_px - 2.0f * padding_px;
  if (usable_width < 0.0f) usable_width = 0.0f;
  if (usable_height < 0.0f) usable_height = 0.0f;

  int columns = static_cast<int>(std::floor(usable_width / metrics.cell_width));
  int rows = static_cast<int>(std::floor(usable_height / metrics.cell_height));
  grid.columns = std::max(1, std::min(columns, kMaxColumns));
  grid.rows = std::max(1, std::min(rows, kMaxRows));
  return grid;
}

}  // namespace render
}  // namespace tmirror
