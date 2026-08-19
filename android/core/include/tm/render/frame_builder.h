#pragma once

#include <cstdint>
#include <vector>

#include "tm/render/atlas.h"
#include "tm/render/metrics.h"
#include "tm/term/snapshot.h"

namespace tmirror {
namespace render {

struct Quad {
  float x = 0.0f;
  float y = 0.0f;
  float width = 0.0f;
  float height = 0.0f;
  Rgba color;
};

struct GlyphQuad {
  float x = 0.0f;
  float y = 0.0f;
  float width = 0.0f;
  float height = 0.0f;
  float u0 = 0.0f, v0 = 0.0f, u1 = 0.0f, v1 = 0.0f;
  int page = 0;
  Rgba color;
};

/// Geometry for one frame, in deterministic layers (spec §10.1): cell backgrounds,
/// then glyphs, then decorations, then the cursor. A backend just uploads and draws
/// these in order; nothing about GL appears here, which is what lets the reference
/// renderer and the GL renderer share every layout decision.
struct RenderFrame {
  std::uint64_t revision = 0;
  std::uint64_t atlas_generation = 0;
  int columns = 0;
  int rows = 0;
  float cell_width = 0.0f;
  float cell_height = 0.0f;
  float width_px = 0.0f;
  float height_px = 0.0f;
  Rgba background;

  std::vector<Quad> backgrounds;
  std::vector<GlyphQuad> glyphs;
  std::vector<Quad> decorations;
  std::vector<Quad> cursor;
  std::vector<GlyphQuad> cursor_glyphs;

  /// True when glyphs were missing from the atlas: the caller rasterizes and draws
  /// again rather than blocking this frame (spec §10.2).
  bool needs_another_frame = false;
};

struct FrameOptions {
  float origin_x = 0.0f;
  float origin_y = 0.0f;
  bool draw_cursor = true;
  bool focused = true;
  /// Phase of the cursor blink and of the SGR blink attribute.
  bool blink_on = true;
};

RenderFrame BuildFrame(const term::Snapshot& snapshot, const Palette& palette,
                       const CellMetrics& metrics, GlyphAtlas* atlas,
                       const FrameOptions& options);

}  // namespace render
}  // namespace tmirror
