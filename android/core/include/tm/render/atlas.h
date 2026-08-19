#pragma once

#include <cstdint>
#include <deque>
#include <map>
#include <memory>
#include <string>
#include <vector>

#include "tm/render/metrics.h"

namespace tmirror {
namespace render {

/// One glyph cluster: a base character plus any combining marks, at a given style.
struct GlyphKey {
  std::u32string cluster;
  bool bold = false;
  bool italic = false;
  int cell_width = 1;

  bool operator<(const GlyphKey& other) const {
    if (cluster != other.cluster) return cluster < other.cluster;
    if (bold != other.bold) return bold < other.bold;
    if (italic != other.italic) return italic < other.italic;
    return cell_width < other.cell_width;
  }
};

/// 8-bit coverage bitmap produced by the platform text stack.
struct GlyphBitmap {
  int width = 0;
  int height = 0;
  /// Offset of the bitmap's top-left corner from the cell origin, in pixels.
  int left = 0;
  int top = 0;
  std::vector<std::uint8_t> alpha;
};

/// Rasterizes glyph clusters.
///
/// On Android this is implemented over `android.graphics` through JNI, which brings
/// system font fallback and shaping with it (spec §10.2). The host build uses a small
/// deterministic bitmap font so golden tests do not depend on installed fonts.
class GlyphRasterizer {
 public:
  virtual ~GlyphRasterizer() = default;
  virtual bool Rasterize(const GlyphKey& key, const CellMetrics& metrics,
                         GlyphBitmap* out) = 0;
  /// Metrics for a font size, so grid sizing does not have to guess.
  virtual CellMetrics MeasureCell(float font_size_px, float density) = 0;
};

struct AtlasEntry {
  bool resident = false;
  int page = 0;
  int x = 0;
  int y = 0;
  int width = 0;
  int height = 0;
  int left = 0;
  int top = 0;
  float u0 = 0.0f, v0 = 0.0f, u1 = 0.0f, v1 = 0.0f;
};

struct AtlasPage {
  std::vector<std::uint8_t> pixels;  // single-channel coverage
  int dirty_x0 = 0, dirty_y0 = 0, dirty_x1 = 0, dirty_y1 = 0;
  bool dirty = false;
};

/// Bounded glyph atlas with shelf packing (spec §10.2, §12: glyph caches are bounded).
///
/// When the atlas fills, it is reset wholesale and its generation counter advances;
/// the renderer notices the change and re-uploads. Evicting individual glyphs from a
/// packed texture would fragment it, and a full reset is bounded, predictable and
/// rare in practice.
class GlyphAtlas {
 public:
  struct Options {
    int page_size = 1024;
    int max_pages = 4;
    /// Bound on glyphs rasterized during a single frame, so a screen full of new
    /// characters cannot stall a frame (spec §10.2).
    std::size_t raster_budget_per_frame = 96;
  };

  GlyphAtlas(Options options, GlyphRasterizer* rasterizer);

  /// Look up a glyph. When it is not resident it is queued and nullptr is returned;
  /// the caller draws nothing for that cell this frame and asks for a redraw once
  /// `ProcessPending` reports progress.
  const AtlasEntry* Lookup(const GlyphKey& key);

  /// Rasterizes queued glyphs, up to the per-frame budget. Returns the number
  /// completed; a non-zero result means another frame should be drawn.
  std::size_t ProcessPending(const CellMetrics& metrics);

  bool has_pending() const { return !pending_.empty(); }
  std::size_t pending_count() const { return pending_.size(); }

  const std::vector<AtlasPage>& pages() const { return pages_; }
  std::vector<AtlasPage>& mutable_pages() { return pages_; }
  int page_size() const { return options_.page_size; }
  /// Advances whenever the atlas is reset; the renderer re-uploads on a change.
  std::uint64_t generation() const { return generation_; }

  void Clear();
  std::size_t resident_glyphs() const { return entries_.size(); }
  std::size_t memory_bytes() const;

 private:
  bool Insert(const GlyphKey& key, const GlyphBitmap& bitmap);
  void StartNewPage();

  Options options_;
  GlyphRasterizer* rasterizer_;
  std::vector<AtlasPage> pages_;
  std::map<GlyphKey, AtlasEntry> entries_;
  std::deque<GlyphKey> pending_;
  int shelf_x_ = 0;
  int shelf_y_ = 0;
  int shelf_height_ = 0;
  std::uint64_t generation_ = 1;
};

/// Deterministic 5x7 bitmap font covering printable ASCII, used by the host build and
/// by golden-image tests. Anything outside its coverage renders as the replacement
/// box, which is exactly what spec §10.2 asks for when a glyph is missing.
class BuiltinFontRasterizer : public GlyphRasterizer {
 public:
  bool Rasterize(const GlyphKey& key, const CellMetrics& metrics, GlyphBitmap* out) override;
  CellMetrics MeasureCell(float font_size_px, float density) override;
};

}  // namespace render
}  // namespace tmirror
