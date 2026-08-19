#pragma once

#include <cstdint>

#include "tm/term/cell.h"

namespace tmirror {
namespace render {

struct Rgba {
  std::uint8_t r = 0;
  std::uint8_t g = 0;
  std::uint8_t b = 0;
  std::uint8_t a = 255;

  bool operator==(const Rgba& other) const {
    return r == other.r && g == other.g && b == other.b && a == other.a;
  }
  bool operator!=(const Rgba& other) const { return !(*this == other); }
};

/// xterm-256 palette plus configurable default foreground/background (spec §8.1).
class Palette {
 public:
  Palette();

  Rgba Resolve(const term::Color& color, bool foreground) const;
  /// Applies inverse, faint and conceal to a resolved pair (spec §8.1).
  void ResolvePair(const term::Cell& cell, bool reverse_video, Rgba* foreground,
                   Rgba* background) const;

  void SetIndexed(int index, Rgba color);
  Rgba indexed(int index) const;
  void set_default_foreground(Rgba color) { default_foreground_ = color; }
  void set_default_background(Rgba color) { default_background_ = color; }
  Rgba default_foreground() const { return default_foreground_; }
  Rgba default_background() const { return default_background_; }
  void set_selection_color(Rgba color) { selection_ = color; }
  Rgba selection_color() const { return selection_; }
  void set_cursor_color(Rgba color) { cursor_ = color; }
  Rgba cursor_color() const { return cursor_; }

  static Rgba Blend(Rgba a, Rgba b, float t);

  /// Minimum foreground/background contrast ratio, in WCAG terms (1.0 disables it).
  ///
  /// Terminal colour schemes are chosen by whoever configured the remote shell, and
  /// some of them are unreadable on a phone in daylight. Spec §13 requires contrast to
  /// be configurable, so a floor can be set here and `ResolvePair` lifts any pair that
  /// falls below it.
  void set_minimum_contrast(float ratio) { minimum_contrast_ = ratio < 1.0f ? 1.0f : ratio; }
  float minimum_contrast() const { return minimum_contrast_; }

  /// WCAG relative luminance and contrast ratio, exposed for tests.
  static float RelativeLuminance(Rgba color);
  static float ContrastRatio(Rgba a, Rgba b);

 private:
  /// Moves `foreground` towards white or black until it clears the floor.
  Rgba EnforceContrast(Rgba foreground, Rgba background) const;

  Rgba palette_[256];
  Rgba default_foreground_;
  Rgba default_background_;
  Rgba selection_;
  Rgba cursor_;
  float minimum_contrast_ = 1.0f;
};

/// Cell geometry in device pixels. The font size in scaled pixels times the display
/// density gives the pixel size; the rasterizer reports the advance and line height.
struct CellMetrics {
  float cell_width = 9.0f;
  float cell_height = 18.0f;
  float baseline = 14.0f;
  float font_size_px = 15.0f;
  float density = 1.0f;
  float underline_thickness = 1.0f;
  float underline_position = 2.0f;

  bool valid() const { return cell_width > 0.5f && cell_height > 0.5f; }
};

struct GridSize {
  int columns = 0;
  int rows = 0;
  bool operator==(const GridSize& other) const {
    return columns == other.columns && rows == other.rows;
  }
  bool operator!=(const GridSize& other) const { return !(*this == other); }
};

/// Number of whole cells that fit in the usable terminal rectangle (spec §10.3).
/// Always at least 1x1: a zero-sized grid would divide by zero downstream, and a
/// transiently zero-sized surface is normal during rotation.
GridSize ComputeGrid(float width_px, float height_px, const CellMetrics& metrics,
                     float padding_px = 0.0f);

/// Maximum grid the client will ever ask for, so a hostile or broken metric cannot
/// produce an enormous request (spec §12).
constexpr int kMaxColumns = 1000;
constexpr int kMaxRows = 1000;

}  // namespace render
}  // namespace tmirror
