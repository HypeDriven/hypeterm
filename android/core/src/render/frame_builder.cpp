#include "tm/render/frame_builder.h"

#include <algorithm>

namespace tmirror {
namespace render {
namespace {

bool CellInSelection(const term::Selection& selection, int row, int column) {
  if (!selection.active) return false;
  int start_row = selection.start_row;
  int start_column = selection.start_column;
  int end_row = selection.end_row;
  int end_column = selection.end_column;
  if (start_row > end_row || (start_row == end_row && start_column > end_column)) {
    std::swap(start_row, end_row);
    std::swap(start_column, end_column);
  }
  if (row < start_row || row > end_row) return false;
  if (selection.rectangular) {
    int low = std::min(start_column, end_column);
    int high = std::max(start_column, end_column);
    return column >= low && column <= high;
  }
  if (row == start_row && column < start_column) return false;
  if (row == end_row && column > end_column) return false;
  return true;
}

}  // namespace

RenderFrame BuildFrame(const term::Snapshot& snapshot, const Palette& palette,
                       const CellMetrics& metrics, GlyphAtlas* atlas,
                       const FrameOptions& options) {
  RenderFrame frame;
  frame.revision = snapshot.revision;
  frame.atlas_generation = atlas != nullptr ? atlas->generation() : 0;
  frame.columns = snapshot.columns;
  frame.rows = snapshot.rows;
  frame.cell_width = metrics.cell_width;
  frame.cell_height = metrics.cell_height;
  frame.width_px = metrics.cell_width * static_cast<float>(snapshot.columns);
  frame.height_px = metrics.cell_height * static_cast<float>(snapshot.rows);
  frame.background =
      snapshot.reverse_video ? palette.default_foreground() : palette.default_background();

  const float cell_width = metrics.cell_width;
  const float cell_height = metrics.cell_height;

  for (int row = 0; row < snapshot.rows; ++row) {
    const term::Line* line = snapshot.line(row);
    if (line == nullptr) continue;
    const float y = options.origin_y + static_cast<float>(row) * cell_height;

    // Backgrounds are emitted as runs so a full-width fill is one quad, not eighty.
    int run_start = -1;
    Rgba run_color{};
    auto flush_run = [&](int end_column) {
      if (run_start < 0) return;
      if (run_color != frame.background) {
        Quad quad;
        quad.x = options.origin_x + static_cast<float>(run_start) * cell_width;
        quad.y = y;
        quad.width = static_cast<float>(end_column - run_start) * cell_width;
        quad.height = cell_height;
        quad.color = run_color;
        frame.backgrounds.push_back(quad);
      }
      run_start = -1;
    };

    for (int column = 0; column < snapshot.columns; ++column) {
      term::Cell cell;
      if (static_cast<std::size_t>(column) < line->size()) {
        cell = line->at(static_cast<std::size_t>(column));
      }
      Rgba foreground;
      Rgba background;
      palette.ResolvePair(cell, snapshot.reverse_video, &foreground, &background);
      if (CellInSelection(snapshot.selection, row, column)) {
        background = palette.selection_color();
        // Keep text readable against the selection tint (spec §13: contrast).
        foreground = Palette::Blend(foreground, palette.default_foreground(), 0.35f);
      }

      if (run_start < 0 || background != run_color) {
        flush_run(column);
        run_start = column;
        run_color = background;
      }

      if (cell.is_continuation()) continue;
      bool blinking = (cell.flags & (term::kFlagBlink | term::kFlagRapidBlink)) != 0;
      bool draw_glyph = cell.code != 0 && cell.code != U' ' && (!blinking || options.blink_on);

      if (draw_glyph && atlas != nullptr) {
        GlyphKey key;
        key.cluster.push_back(cell.code);
        if (cell.has_marks()) {
          const std::u32string* marks = line->Marks(static_cast<std::size_t>(column));
          if (marks != nullptr) key.cluster.append(*marks);
        }
        key.bold = (cell.flags & term::kFlagBold) != 0;
        key.italic = (cell.flags & term::kFlagItalic) != 0;
        key.cell_width = cell.width == 2 ? 2 : 1;

        const AtlasEntry* entry = atlas->Lookup(key);
        if (entry == nullptr) {
          frame.needs_another_frame = true;
        } else if (entry->resident && entry->width > 0) {
          GlyphQuad quad;
          quad.x = options.origin_x + static_cast<float>(column) * cell_width +
                   static_cast<float>(entry->left);
          quad.y = y + static_cast<float>(entry->top);
          quad.width = static_cast<float>(entry->width);
          quad.height = static_cast<float>(entry->height);
          quad.u0 = entry->u0;
          quad.v0 = entry->v0;
          quad.u1 = entry->u1;
          quad.v1 = entry->v1;
          quad.page = entry->page;
          quad.color = foreground;
          frame.glyphs.push_back(quad);
        }
      }

      const float x = options.origin_x + static_cast<float>(column) * cell_width;
      const float span = cell_width * static_cast<float>(cell.width == 2 ? 2 : 1);
      Rgba decoration_color = foreground;
      if (!cell.underline_color.is_default()) {
        decoration_color = palette.Resolve(cell.underline_color, true);
      }
      if ((cell.flags & term::kFlagAnyUnderline) != 0) {
        Quad quad;
        quad.x = x;
        quad.y = y + metrics.baseline + metrics.underline_position;
        quad.width = span;
        quad.height = metrics.underline_thickness;
        quad.color = decoration_color;
        frame.decorations.push_back(quad);
        if ((cell.flags & term::kFlagDoubleUnderline) != 0) {
          quad.y += metrics.underline_thickness * 2.0f;
          frame.decorations.push_back(quad);
        }
      }
      if ((cell.flags & term::kFlagStrike) != 0) {
        Quad quad;
        quad.x = x;
        quad.y = y + cell_height * 0.5f;
        quad.width = span;
        quad.height = metrics.underline_thickness;
        quad.color = foreground;
        frame.decorations.push_back(quad);
      }
      if ((cell.flags & term::kFlagOverline) != 0) {
        Quad quad;
        quad.x = x;
        quad.y = y;
        quad.width = span;
        quad.height = metrics.underline_thickness;
        quad.color = foreground;
        frame.decorations.push_back(quad);
      }
    }
    flush_run(snapshot.columns);
  }

  // The cursor is drawn last, and a block cursor redraws the glyph beneath it in the
  // background colour so the character stays legible.
  const term::CursorState& cursor = snapshot.cursor;
  if (options.draw_cursor && cursor.visible && cursor.row >= 0 && cursor.row < snapshot.rows &&
      cursor.column >= 0 && cursor.column < snapshot.columns &&
      (options.blink_on || !cursor.blinking)) {
    const float x = options.origin_x + static_cast<float>(cursor.column) * cell_width;
    const float y = options.origin_y + static_cast<float>(cursor.row) * cell_height;
    Quad quad;
    quad.color = palette.cursor_color();
    switch (cursor.shape) {
      case term::CursorShape::kBlock:
        quad.x = x;
        quad.y = y;
        quad.width = cell_width;
        quad.height = cell_height;
        break;
      case term::CursorShape::kUnderline:
        quad.x = x;
        quad.y = y + cell_height - std::max(2.0f, metrics.underline_thickness * 2.0f);
        quad.width = cell_width;
        quad.height = std::max(2.0f, metrics.underline_thickness * 2.0f);
        break;
      case term::CursorShape::kBar:
        quad.x = x;
        quad.y = y;
        quad.width = std::max(2.0f, metrics.underline_thickness * 2.0f);
        quad.height = cell_height;
        break;
    }

    if (!options.focused && cursor.shape == term::CursorShape::kBlock) {
      // An unfocused terminal shows a hollow cursor, which is also the accessible
      // signal that typing will not go anywhere (spec §13).
      const float thickness = std::max(1.0f, metrics.underline_thickness);
      Quad top = quad;
      top.height = thickness;
      Quad bottom = quad;
      bottom.y = y + cell_height - thickness;
      bottom.height = thickness;
      Quad left = quad;
      left.width = thickness;
      Quad right = quad;
      right.x = x + cell_width - thickness;
      right.width = thickness;
      frame.cursor.push_back(top);
      frame.cursor.push_back(bottom);
      frame.cursor.push_back(left);
      frame.cursor.push_back(right);
    } else {
      frame.cursor.push_back(quad);
      if (cursor.shape == term::CursorShape::kBlock && atlas != nullptr) {
        const term::Line* line = snapshot.line(cursor.row);
        if (line != nullptr &&
            static_cast<std::size_t>(cursor.column) < line->size()) {
          const term::Cell& cell = line->at(static_cast<std::size_t>(cursor.column));
          if (cell.code != 0 && cell.code != U' ') {
            GlyphKey key;
            key.cluster.push_back(cell.code);
            key.bold = (cell.flags & term::kFlagBold) != 0;
            key.italic = (cell.flags & term::kFlagItalic) != 0;
            key.cell_width = cell.width == 2 ? 2 : 1;
            const AtlasEntry* entry = atlas->Lookup(key);
            if (entry != nullptr && entry->resident && entry->width > 0) {
              Rgba foreground;
              Rgba background;
              palette.ResolvePair(cell, snapshot.reverse_video, &foreground, &background);
              GlyphQuad glyph;
              glyph.x = x + static_cast<float>(entry->left);
              glyph.y = y + static_cast<float>(entry->top);
              glyph.width = static_cast<float>(entry->width);
              glyph.height = static_cast<float>(entry->height);
              glyph.u0 = entry->u0;
              glyph.v0 = entry->v0;
              glyph.u1 = entry->u1;
              glyph.v1 = entry->v1;
              glyph.page = entry->page;
              glyph.color = background;
              frame.cursor_glyphs.push_back(glyph);
            } else if (entry == nullptr) {
              frame.needs_another_frame = true;
            }
          }
        }
      }
    }
  }

  return frame;
}

}  // namespace render
}  // namespace tmirror
