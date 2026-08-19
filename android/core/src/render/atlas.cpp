#include "tm/render/atlas.h"

#include <algorithm>
#include <cmath>

namespace tmirror {
namespace render {
namespace {

/// Classic 5x7 cell font, five column bytes per glyph, bit 0 at the top. Printable
/// ASCII only: anything else falls back to the replacement box.
constexpr std::uint8_t kFont5x7[95][5] = {
    {0x00, 0x00, 0x00, 0x00, 0x00}, {0x00, 0x00, 0x5F, 0x00, 0x00},
    {0x00, 0x07, 0x00, 0x07, 0x00}, {0x14, 0x7F, 0x14, 0x7F, 0x14},
    {0x24, 0x2A, 0x7F, 0x2A, 0x12}, {0x23, 0x13, 0x08, 0x64, 0x62},
    {0x36, 0x49, 0x55, 0x22, 0x50}, {0x00, 0x05, 0x03, 0x00, 0x00},
    {0x00, 0x1C, 0x22, 0x41, 0x00}, {0x00, 0x41, 0x22, 0x1C, 0x00},
    {0x14, 0x08, 0x3E, 0x08, 0x14}, {0x08, 0x08, 0x3E, 0x08, 0x08},
    {0x00, 0x50, 0x30, 0x00, 0x00}, {0x08, 0x08, 0x08, 0x08, 0x08},
    {0x00, 0x60, 0x60, 0x00, 0x00}, {0x20, 0x10, 0x08, 0x04, 0x02},
    {0x3E, 0x51, 0x49, 0x45, 0x3E}, {0x00, 0x42, 0x7F, 0x40, 0x00},
    {0x42, 0x61, 0x51, 0x49, 0x46}, {0x21, 0x41, 0x45, 0x4B, 0x31},
    {0x18, 0x14, 0x12, 0x7F, 0x10}, {0x27, 0x45, 0x45, 0x45, 0x39},
    {0x3C, 0x4A, 0x49, 0x49, 0x30}, {0x01, 0x71, 0x09, 0x05, 0x03},
    {0x36, 0x49, 0x49, 0x49, 0x36}, {0x06, 0x49, 0x49, 0x29, 0x1E},
    {0x00, 0x36, 0x36, 0x00, 0x00}, {0x00, 0x56, 0x36, 0x00, 0x00},
    {0x08, 0x14, 0x22, 0x41, 0x00}, {0x14, 0x14, 0x14, 0x14, 0x14},
    {0x00, 0x41, 0x22, 0x14, 0x08}, {0x02, 0x01, 0x51, 0x09, 0x06},
    {0x32, 0x49, 0x79, 0x41, 0x3E}, {0x7E, 0x11, 0x11, 0x11, 0x7E},
    {0x7F, 0x49, 0x49, 0x49, 0x36}, {0x3E, 0x41, 0x41, 0x41, 0x22},
    {0x7F, 0x41, 0x41, 0x22, 0x1C}, {0x7F, 0x49, 0x49, 0x49, 0x41},
    {0x7F, 0x09, 0x09, 0x09, 0x01}, {0x3E, 0x41, 0x49, 0x49, 0x7A},
    {0x7F, 0x08, 0x08, 0x08, 0x7F}, {0x00, 0x41, 0x7F, 0x41, 0x00},
    {0x20, 0x40, 0x41, 0x3F, 0x01}, {0x7F, 0x08, 0x14, 0x22, 0x41},
    {0x7F, 0x40, 0x40, 0x40, 0x40}, {0x7F, 0x02, 0x0C, 0x02, 0x7F},
    {0x7F, 0x04, 0x08, 0x10, 0x7F}, {0x3E, 0x41, 0x41, 0x41, 0x3E},
    {0x7F, 0x09, 0x09, 0x09, 0x06}, {0x3E, 0x41, 0x51, 0x21, 0x5E},
    {0x7F, 0x09, 0x19, 0x29, 0x46}, {0x46, 0x49, 0x49, 0x49, 0x31},
    {0x01, 0x01, 0x7F, 0x01, 0x01}, {0x3F, 0x40, 0x40, 0x40, 0x3F},
    {0x1F, 0x20, 0x40, 0x20, 0x1F}, {0x3F, 0x40, 0x38, 0x40, 0x3F},
    {0x63, 0x14, 0x08, 0x14, 0x63}, {0x07, 0x08, 0x70, 0x08, 0x07},
    {0x61, 0x51, 0x49, 0x45, 0x43}, {0x00, 0x7F, 0x41, 0x41, 0x00},
    {0x02, 0x04, 0x08, 0x10, 0x20}, {0x00, 0x41, 0x41, 0x7F, 0x00},
    {0x04, 0x02, 0x01, 0x02, 0x04}, {0x40, 0x40, 0x40, 0x40, 0x40},
    {0x00, 0x01, 0x02, 0x04, 0x00}, {0x20, 0x54, 0x54, 0x54, 0x78},
    {0x7F, 0x48, 0x44, 0x44, 0x38}, {0x38, 0x44, 0x44, 0x44, 0x20},
    {0x38, 0x44, 0x44, 0x48, 0x7F}, {0x38, 0x54, 0x54, 0x54, 0x18},
    {0x08, 0x7E, 0x09, 0x01, 0x02}, {0x0C, 0x52, 0x52, 0x52, 0x3E},
    {0x7F, 0x08, 0x04, 0x04, 0x78}, {0x00, 0x44, 0x7D, 0x40, 0x00},
    {0x20, 0x40, 0x44, 0x3D, 0x00}, {0x7F, 0x10, 0x28, 0x44, 0x00},
    {0x00, 0x41, 0x7F, 0x40, 0x00}, {0x7C, 0x04, 0x18, 0x04, 0x78},
    {0x7C, 0x08, 0x04, 0x04, 0x78}, {0x38, 0x44, 0x44, 0x44, 0x38},
    {0x7C, 0x14, 0x14, 0x14, 0x08}, {0x08, 0x14, 0x14, 0x18, 0x7C},
    {0x7C, 0x08, 0x04, 0x04, 0x08}, {0x48, 0x54, 0x54, 0x54, 0x20},
    {0x04, 0x3F, 0x44, 0x40, 0x20}, {0x3C, 0x40, 0x40, 0x20, 0x7C},
    {0x1C, 0x20, 0x40, 0x20, 0x1C}, {0x3C, 0x40, 0x30, 0x40, 0x3C},
    {0x44, 0x28, 0x10, 0x28, 0x44}, {0x0C, 0x50, 0x50, 0x50, 0x3C},
    {0x44, 0x64, 0x54, 0x4C, 0x44}, {0x00, 0x08, 0x36, 0x41, 0x00},
    {0x00, 0x00, 0x7F, 0x00, 0x00}, {0x00, 0x41, 0x36, 0x08, 0x00},
    {0x08, 0x04, 0x08, 0x10, 0x08},
};

constexpr int kPadding = 1;

}  // namespace

GlyphAtlas::GlyphAtlas(Options options, GlyphRasterizer* rasterizer)
    : options_(options), rasterizer_(rasterizer) {
  if (options_.page_size < 64) options_.page_size = 64;
  if (options_.max_pages < 1) options_.max_pages = 1;
  StartNewPage();
}

void GlyphAtlas::StartNewPage() {
  AtlasPage page;
  page.pixels.assign(static_cast<std::size_t>(options_.page_size) *
                         static_cast<std::size_t>(options_.page_size),
                     0);
  pages_.push_back(std::move(page));
  shelf_x_ = 0;
  shelf_y_ = 0;
  shelf_height_ = 0;
}

void GlyphAtlas::Clear() {
  pages_.clear();
  entries_.clear();
  pending_.clear();
  StartNewPage();
  ++generation_;
}

std::size_t GlyphAtlas::memory_bytes() const {
  return pages_.size() * static_cast<std::size_t>(options_.page_size) *
         static_cast<std::size_t>(options_.page_size);
}

const AtlasEntry* GlyphAtlas::Lookup(const GlyphKey& key) {
  auto it = entries_.find(key);
  if (it != entries_.end()) return it->second.resident ? &it->second : nullptr;

  // Queue it, bounded: a screen full of unseen glyphs must not create an unbounded
  // backlog (spec §12).
  if (pending_.size() < 4096) {
    bool already_queued = false;
    for (const GlyphKey& queued : pending_) {
      if (!(queued < key) && !(key < queued)) {
        already_queued = true;
        break;
      }
    }
    if (!already_queued) pending_.push_back(key);
  }
  return nullptr;
}

std::size_t GlyphAtlas::ProcessPending(const CellMetrics& metrics) {
  if (rasterizer_ == nullptr) {
    pending_.clear();
    return 0;
  }
  std::size_t done = 0;
  while (!pending_.empty() && done < options_.raster_budget_per_frame) {
    GlyphKey key = pending_.front();
    pending_.pop_front();
    if (entries_.find(key) != entries_.end()) continue;

    GlyphBitmap bitmap;
    if (!rasterizer_->Rasterize(key, metrics, &bitmap)) {
      // Record the failure so the same glyph is not retried every frame.
      AtlasEntry entry;
      entry.resident = false;
      entries_[key] = entry;
      continue;
    }
    if (!Insert(key, bitmap)) {
      // Out of atlas space: reset and retry this glyph next frame.
      Clear();
      pending_.push_front(key);
      break;
    }
    ++done;
  }
  return done;
}

bool GlyphAtlas::Insert(const GlyphKey& key, const GlyphBitmap& bitmap) {
  const int page_size = options_.page_size;
  int width = bitmap.width;
  int height = bitmap.height;
  if (width <= 0 || height <= 0) {
    AtlasEntry entry;
    entry.resident = true;
    entry.page = static_cast<int>(pages_.size()) - 1;
    entries_[key] = entry;
    return true;
  }
  if (width + 2 * kPadding > page_size || height + 2 * kPadding > page_size) return false;

  if (shelf_x_ + width + kPadding > page_size) {
    shelf_x_ = 0;
    shelf_y_ += shelf_height_ + kPadding;
    shelf_height_ = 0;
  }
  if (shelf_y_ + height + kPadding > page_size) {
    if (static_cast<int>(pages_.size()) >= options_.max_pages) return false;
    StartNewPage();
  }

  AtlasPage& page = pages_.back();
  const int x = shelf_x_;
  const int y = shelf_y_;
  for (int row = 0; row < height; ++row) {
    for (int column = 0; column < width; ++column) {
      std::size_t source = static_cast<std::size_t>(row) * static_cast<std::size_t>(width) +
                           static_cast<std::size_t>(column);
      std::size_t destination =
          static_cast<std::size_t>(y + row) * static_cast<std::size_t>(page_size) +
          static_cast<std::size_t>(x + column);
      page.pixels[destination] = bitmap.alpha[source];
    }
  }
  if (!page.dirty) {
    page.dirty = true;
    page.dirty_x0 = x;
    page.dirty_y0 = y;
    page.dirty_x1 = x + width;
    page.dirty_y1 = y + height;
  } else {
    page.dirty_x0 = std::min(page.dirty_x0, x);
    page.dirty_y0 = std::min(page.dirty_y0, y);
    page.dirty_x1 = std::max(page.dirty_x1, x + width);
    page.dirty_y1 = std::max(page.dirty_y1, y + height);
  }

  AtlasEntry entry;
  entry.resident = true;
  entry.page = static_cast<int>(pages_.size()) - 1;
  entry.x = x;
  entry.y = y;
  entry.width = width;
  entry.height = height;
  entry.left = bitmap.left;
  entry.top = bitmap.top;
  const float size = static_cast<float>(page_size);
  entry.u0 = static_cast<float>(x) / size;
  entry.v0 = static_cast<float>(y) / size;
  entry.u1 = static_cast<float>(x + width) / size;
  entry.v1 = static_cast<float>(y + height) / size;
  entries_[key] = entry;

  shelf_x_ += width + kPadding;
  shelf_height_ = std::max(shelf_height_, height);
  return true;
}

// ------------------------------------------------------------ builtin rasterizer

CellMetrics BuiltinFontRasterizer::MeasureCell(float font_size_px, float density) {
  CellMetrics metrics;
  metrics.font_size_px = font_size_px;
  metrics.density = density;
  metrics.cell_width = std::max(4.0f, std::floor(font_size_px * 0.6f));
  metrics.cell_height = std::max(6.0f, std::floor(font_size_px * 1.25f));
  metrics.baseline = std::floor(metrics.cell_height * 0.8f);
  metrics.underline_thickness = std::max(1.0f, std::floor(font_size_px / 14.0f));
  metrics.underline_position = std::max(1.0f, std::floor(metrics.cell_height * 0.12f));
  return metrics;
}

bool BuiltinFontRasterizer::Rasterize(const GlyphKey& key, const CellMetrics& metrics,
                                      GlyphBitmap* out) {
  if (key.cluster.empty()) return false;
  char32_t code = key.cluster[0];
  if (code == U' ' || code == 0) return false;

  const int cell_width = static_cast<int>(metrics.cell_width) * key.cell_width;
  const int cell_height = static_cast<int>(metrics.cell_height);
  if (cell_width <= 0 || cell_height <= 0) return false;

  const int scale =
      std::max(1, static_cast<int>(std::floor(std::min(static_cast<float>(cell_width) / 6.0f,
                                                       static_cast<float>(cell_height) / 9.0f))));
  const int glyph_width = 5 * scale;
  const int glyph_height = 7 * scale;

  out->width = glyph_width + (key.bold ? scale : 0);
  out->height = glyph_height;
  out->alpha.assign(static_cast<std::size_t>(out->width) *
                        static_cast<std::size_t>(out->height),
                    0);
  out->left = (cell_width - out->width) / 2;
  out->top = static_cast<int>(metrics.baseline) - glyph_height;
  if (out->top < 0) out->top = 0;

  auto plot = [&](int x, int y) {
    if (x < 0 || y < 0 || x >= out->width || y >= out->height) return;
    out->alpha[static_cast<std::size_t>(y) * static_cast<std::size_t>(out->width) +
               static_cast<std::size_t>(x)] = 255;
  };

  if (code >= 0x20 && code <= 0x7E) {
    const std::uint8_t* columns = kFont5x7[code - 0x20];
    for (int column = 0; column < 5; ++column) {
      for (int row = 0; row < 7; ++row) {
        if ((columns[column] & (1u << row)) == 0) continue;
        for (int sy = 0; sy < scale; ++sy) {
          for (int sx = 0; sx < scale; ++sx) {
            int x = column * scale + sx;
            int y = row * scale + sy;
            // Italic shears the upper rows to the right.
            if (key.italic) x += (glyph_height - y) / 4;
            plot(x, y);
            if (key.bold) plot(x + scale, y);
          }
        }
      }
    }
  } else {
    // Replacement box for anything the builtin font does not cover (spec §10.2).
    for (int x = 0; x < glyph_width; ++x) {
      for (int t = 0; t < scale; ++t) {
        plot(x, t);
        plot(x, glyph_height - 1 - t);
      }
    }
    for (int y = 0; y < glyph_height; ++y) {
      for (int t = 0; t < scale; ++t) {
        plot(t, y);
        plot(glyph_width - 1 - t, y);
      }
    }
  }
  return true;
}

}  // namespace render
}  // namespace tmirror
