// Renderer geometry, palette resolution, frame layering and golden images
// (spec §10, §13, §16.3).

#include <string>

#include "framework.h"
#include "helpers.h"
#include "tm/render/atlas.h"
#include "tm/render/frame_builder.h"
#include "tm/render/metrics.h"
#include "tm/render/reference_renderer.h"
#include "tm/term/emulator.h"

using tmirror::render::AtlasEntry;
using tmirror::render::BuildFrame;
using tmirror::render::BuiltinFontRasterizer;
using tmirror::render::CellMetrics;
using tmirror::render::ComputeGrid;
using tmirror::render::FrameOptions;
using tmirror::render::GlyphAtlas;
using tmirror::render::GlyphKey;
using tmirror::render::GridSize;
using tmirror::render::Palette;
using tmirror::render::ReferenceRenderer;
using tmirror::render::RenderFrame;
using tmirror::render::Rgba;
using tmirror::term::Emulator;
using tmirror::term::Snapshot;
using tmtest::Feed;
using tmtest::SmallConfig;

namespace {

CellMetrics TestMetrics() {
  BuiltinFontRasterizer rasterizer;
  return rasterizer.MeasureCell(16.0f, 1.0f);
}

/// Builds a frame, rasterizing everything the first pass found missing, so a test
/// gets a complete image rather than the first-frame partial one.
RenderFrame BuildComplete(const Snapshot& snapshot, const Palette& palette,
                          const CellMetrics& metrics, GlyphAtlas* atlas,
                          const FrameOptions& options) {
  RenderFrame frame = BuildFrame(snapshot, palette, metrics, atlas, options);
  for (int attempt = 0; attempt < 8 && frame.needs_another_frame; ++attempt) {
    atlas->ProcessPending(metrics);
    frame = BuildFrame(snapshot, palette, metrics, atlas, options);
  }
  return frame;
}

}  // namespace

TM_TEST(Render, GridSizeFloorsToWholeCells) {
  CellMetrics metrics;
  metrics.cell_width = 10.0f;
  metrics.cell_height = 20.0f;
  GridSize grid = ComputeGrid(105.0f, 210.0f, metrics);
  TM_CHECK_EQ(grid.columns, 10);
  TM_CHECK_EQ(grid.rows, 10);

  // Padding is taken from both sides.
  grid = ComputeGrid(105.0f, 210.0f, metrics, 10.0f);
  TM_CHECK_EQ(grid.columns, 8);
  TM_CHECK_EQ(grid.rows, 9);
}

TM_TEST(Render, GridSizeIsNeverZeroOrAbsurd) {
  CellMetrics metrics;
  metrics.cell_width = 10.0f;
  metrics.cell_height = 20.0f;
  // A transiently zero-sized surface is normal during rotation.
  GridSize tiny = ComputeGrid(0.0f, 0.0f, metrics);
  TM_CHECK_EQ(tiny.columns, 1);
  TM_CHECK_EQ(tiny.rows, 1);

  GridSize huge = ComputeGrid(1e9f, 1e9f, metrics);
  TM_CHECK(huge.columns <= tmirror::render::kMaxColumns);
  TM_CHECK(huge.rows <= tmirror::render::kMaxRows);

  CellMetrics degenerate;
  degenerate.cell_width = 0.0f;
  degenerate.cell_height = 0.0f;
  GridSize safe = ComputeGrid(100.0f, 100.0f, degenerate);
  TM_CHECK_EQ(safe.columns, 1);
}

TM_TEST(Render, PaletteResolvesEveryColourKind) {
  Palette palette;
  TM_CHECK(palette.Resolve(tmirror::term::Color::Default(), true) ==
           palette.default_foreground());
  TM_CHECK(palette.Resolve(tmirror::term::Color::Indexed(1), true) == palette.indexed(1));
  Rgba direct = palette.Resolve(tmirror::term::Color::Rgb(1, 2, 3), true);
  TM_CHECK_EQ(static_cast<int>(direct.r), 1);
  TM_CHECK_EQ(static_cast<int>(direct.g), 2);
  TM_CHECK_EQ(static_cast<int>(direct.b), 3);

  // The 256-colour cube and the grey ramp are where off-by-one errors hide.
  Rgba cube = palette.indexed(16);
  TM_CHECK_EQ(static_cast<int>(cube.r), 0);
  Rgba white_cube = palette.indexed(231);
  TM_CHECK_EQ(static_cast<int>(white_cube.r), 255);
  Rgba grey = palette.indexed(232);
  TM_CHECK_EQ(static_cast<int>(grey.r), 8);
}

TM_TEST(Render, InverseAndFaintAndConcealAreApplied) {
  Palette palette;
  tmirror::term::Cell cell;
  cell.fg = tmirror::term::Color::Indexed(1);
  cell.bg = tmirror::term::Color::Indexed(2);

  Rgba foreground;
  Rgba background;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(foreground == palette.indexed(1));

  cell.flags = tmirror::term::kFlagInverse;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(foreground == palette.indexed(2));
  TM_CHECK(background == palette.indexed(1));

  // Reverse video inverts again, so the two cancel out.
  palette.ResolvePair(cell, true, &foreground, &background);
  TM_CHECK(foreground == palette.indexed(1));

  cell.flags = tmirror::term::kFlagConceal;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(foreground == background);

  // Bold brightens the low palette colours but never a direct colour.
  cell.flags = tmirror::term::kFlagBold;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(foreground == palette.indexed(9));
}

TM_TEST(Render, MinimumContrastIsEnforcedWhenConfigured) {
  Palette palette;
  palette.set_default_background(Rgba{0, 0, 0, 255});

  tmirror::term::Cell cell;
  // Dark grey on black: legible on a desk, not on a phone in daylight.
  cell.fg = tmirror::term::Color::Rgb(40, 40, 40);
  cell.bg = tmirror::term::Color::Rgb(0, 0, 0);

  Rgba foreground;
  Rgba background;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(Palette::ContrastRatio(foreground, background) < 3.0f);

  palette.set_minimum_contrast(4.5f);
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK_MSG(Palette::ContrastRatio(foreground, background) >= 4.4f,
               "contrast was " +
                   std::to_string(Palette::ContrastRatio(foreground, background)));

  // A pair that already clears the floor is left exactly as it was.
  cell.fg = tmirror::term::Color::Rgb(255, 255, 255);
  Rgba untouched_foreground;
  palette.ResolvePair(cell, false, &untouched_foreground, &background);
  TM_CHECK(untouched_foreground == Rgba({255, 255, 255, 255}));

  // Concealed text stays invisible whatever the contrast floor says.
  cell.flags = tmirror::term::kFlagConceal;
  palette.ResolvePair(cell, false, &foreground, &background);
  TM_CHECK(foreground == background);
}

TM_TEST(Render, ContrastRatioMatchesTheWcagDefinition) {
  // Black on white is the canonical 21:1.
  float extreme = Palette::ContrastRatio(Rgba{0, 0, 0, 255}, Rgba{255, 255, 255, 255});
  TM_CHECK(extreme > 20.9f && extreme < 21.1f);
  float identical = Palette::ContrastRatio(Rgba{80, 90, 100, 255}, Rgba{80, 90, 100, 255});
  TM_CHECK(identical > 0.99f && identical < 1.01f);
}

TM_TEST(Render, AtlasRasterizesLazilyWithinItsBudget) {
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas::Options options;
  options.raster_budget_per_frame = 2;
  GlyphAtlas atlas(options, &rasterizer);

  GlyphKey a;
  a.cluster = U"a";
  TM_CHECK(atlas.Lookup(a) == nullptr);  // queued, not blocking
  TM_CHECK(atlas.has_pending());
  TM_CHECK_EQ(atlas.ProcessPending(TestMetrics()), static_cast<std::size_t>(1));
  const AtlasEntry* entry = atlas.Lookup(a);
  TM_REQUIRE(entry != nullptr);
  TM_CHECK(entry->resident);
  TM_CHECK(entry->width > 0);

  GlyphKey b;
  b.cluster = U"b";
  GlyphKey c;
  c.cluster = U"c";
  GlyphKey d;
  d.cluster = U"d";
  atlas.Lookup(b);
  atlas.Lookup(c);
  atlas.Lookup(d);
  // The budget bounds per-frame work; the rest stays queued (spec §10.2).
  TM_CHECK_EQ(atlas.ProcessPending(TestMetrics()), static_cast<std::size_t>(2));
  TM_CHECK(atlas.has_pending());
}

TM_TEST(Render, AtlasMemoryIsBoundedAndResetsWhenFull) {
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas::Options options;
  options.page_size = 64;
  options.max_pages = 1;
  options.raster_budget_per_frame = 1000;
  GlyphAtlas atlas(options, &rasterizer);

  std::uint64_t generation = atlas.generation();
  CellMetrics metrics = TestMetrics();
  for (char32_t c = U'!'; c <= U'~'; ++c) {
    GlyphKey key;
    key.cluster.push_back(c);
    atlas.Lookup(key);
    atlas.ProcessPending(metrics);
  }
  TM_CHECK(atlas.memory_bytes() <= 64 * 64);
  // Filling it forces a reset rather than unbounded growth.
  TM_CHECK(atlas.generation() > generation);
}

TM_TEST(Render, MissingGlyphsGetTheReplacementBox) {
  BuiltinFontRasterizer rasterizer;
  tmirror::render::GlyphBitmap bitmap;
  GlyphKey key;
  key.cluster = std::u32string(1, 0x4E00);  // outside the builtin font
  key.cell_width = 2;
  TM_CHECK(rasterizer.Rasterize(key, TestMetrics(), &bitmap));
  TM_CHECK(bitmap.width > 0);
  bool has_coverage = false;
  for (std::uint8_t value : bitmap.alpha) {
    if (value != 0) has_coverage = true;
  }
  TM_CHECK(has_coverage);
}

TM_TEST(Render, FrameLayersBackgroundsGlyphsAndCursor) {
  Emulator emulator(SmallConfig(8, 2));
  Feed(emulator, "\x1b[41mab\x1b[0mcd");
  Snapshot snapshot = emulator.BuildSnapshot(0);

  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  FrameOptions options;
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, options);

  TM_CHECK_EQ(frame.columns, 8);
  TM_CHECK_EQ(frame.rows, 2);
  // Two red cells become one background run.
  TM_CHECK(!frame.backgrounds.empty());
  TM_CHECK(frame.backgrounds[0].color == palette.indexed(1));
  TM_CHECK_EQ(frame.backgrounds[0].width, metrics.cell_width * 2.0f);
  // Four glyphs, and a cursor quad.
  TM_CHECK_EQ(frame.glyphs.size(), static_cast<std::size_t>(4));
  TM_CHECK(!frame.cursor.empty());
}

TM_TEST(Render, DecorationsFollowTheirAttributes) {
  Emulator emulator(SmallConfig(6, 1));
  Feed(emulator, "\x1b[4;9;53ma");
  Snapshot snapshot = emulator.BuildSnapshot(0);
  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, FrameOptions());
  // Underline, strike-through and overline are three separate decoration quads.
  TM_CHECK_EQ(frame.decorations.size(), static_cast<std::size_t>(3));
}

TM_TEST(Render, SelectionTintsTheSelectedCells) {
  Emulator emulator(SmallConfig(6, 1));
  Feed(emulator, "abcdef");
  tmirror::term::Selection selection;
  selection.active = true;
  selection.start_row = 0;
  selection.start_column = 1;
  selection.end_row = 0;
  selection.end_column = 3;
  Snapshot snapshot = emulator.BuildSnapshot(0, selection);

  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, FrameOptions());

  bool found = false;
  for (const auto& quad : frame.backgrounds) {
    if (quad.color == palette.selection_color()) {
      TM_CHECK_EQ(quad.width, metrics.cell_width * 3.0f);
      found = true;
    }
  }
  TM_CHECK(found);
}

TM_TEST(Render, HiddenCursorProducesNoCursorQuad) {
  Emulator emulator(SmallConfig(6, 1));
  Feed(emulator, "\x1b[?25l");
  Snapshot snapshot = emulator.BuildSnapshot(0);
  Palette palette;
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  RenderFrame frame = BuildFrame(snapshot, palette, TestMetrics(), &atlas, FrameOptions());
  TM_CHECK(frame.cursor.empty());
}

TM_TEST(Render, UnfocusedCursorIsHollow) {
  Emulator emulator(SmallConfig(6, 1));
  Snapshot snapshot = emulator.BuildSnapshot(0);
  Palette palette;
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  FrameOptions options;
  options.focused = false;
  RenderFrame frame = BuildFrame(snapshot, palette, TestMetrics(), &atlas, options);
  // Four edge quads rather than one filled block, which is also the non-colour
  // signal that typing goes nowhere (spec §13).
  TM_CHECK_EQ(frame.cursor.size(), static_cast<std::size_t>(4));
}

TM_TEST(Render, WideCharactersSpanTwoCells) {
  Emulator emulator(SmallConfig(6, 1));
  Feed(emulator, "\xE4\xB8\x80\x1b[4m\xE4\xB8\x80");
  Snapshot snapshot = emulator.BuildSnapshot(0);
  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, FrameOptions());
  TM_REQUIRE(!frame.decorations.empty());
  TM_CHECK_EQ(frame.decorations[0].width, metrics.cell_width * 2.0f);
}

TM_TEST(Render, ReferenceRendererIsDeterministic) {
  Emulator emulator(SmallConfig(10, 3));
  Feed(emulator, "\x1b[31mhello\x1b[0m\r\nworld");
  Snapshot snapshot = emulator.BuildSnapshot(0);

  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, FrameOptions());

  int width = static_cast<int>(metrics.cell_width * 10.0f);
  int height = static_cast<int>(metrics.cell_height * 3.0f);
  ReferenceRenderer::Image first = ReferenceRenderer::Render(frame, atlas, width, height);
  ReferenceRenderer::Image second = ReferenceRenderer::Render(frame, atlas, width, height);
  TM_CHECK_EQ(ReferenceRenderer::Fingerprint(first), ReferenceRenderer::Fingerprint(second));

  // The same terminal content rendered through a fresh emulator and atlas produces
  // the same image: this is what makes a golden comparison meaningful.
  Emulator again(SmallConfig(10, 3));
  Feed(again, "\x1b[31mhello\x1b[0m\r\nworld");
  GlyphAtlas fresh_atlas(GlyphAtlas::Options(), &rasterizer);
  Snapshot again_snapshot = again.BuildSnapshot(0);
  RenderFrame again_frame =
      BuildComplete(again_snapshot, palette, metrics, &fresh_atlas, FrameOptions());
  ReferenceRenderer::Image again_image =
      ReferenceRenderer::Render(again_frame, fresh_atlas, width, height);
  TM_CHECK_EQ(ReferenceRenderer::Fingerprint(first),
              ReferenceRenderer::Fingerprint(again_image));
}

TM_TEST(Render, ReferenceRendererPaintsWhatTheFrameDescribes) {
  Emulator emulator(SmallConfig(4, 1));
  Feed(emulator, "\x1b[41m  \x1b[0m");
  Snapshot snapshot = emulator.BuildSnapshot(0);
  Palette palette;
  CellMetrics metrics = TestMetrics();
  BuiltinFontRasterizer rasterizer;
  GlyphAtlas atlas(GlyphAtlas::Options(), &rasterizer);
  FrameOptions options;
  options.draw_cursor = false;
  RenderFrame frame = BuildComplete(snapshot, palette, metrics, &atlas, options);

  int width = static_cast<int>(metrics.cell_width * 4.0f);
  int height = static_cast<int>(metrics.cell_height);
  ReferenceRenderer::Image image = ReferenceRenderer::Render(frame, atlas, width, height);
  TM_CHECK(image.At(1, 1) == palette.indexed(1));                 // inside the red run
  TM_CHECK(image.At(width - 1, height - 1) == frame.background);  // outside it
}
