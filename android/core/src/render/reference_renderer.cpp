#include "tm/render/reference_renderer.h"

#include <algorithm>
#include <cmath>
#include <cstdio>

#include "tm/crypto/crypto.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace render {
namespace {

std::uint8_t BlendChannel(std::uint8_t destination, std::uint8_t source, float alpha) {
  float value = static_cast<float>(destination) * (1.0f - alpha) +
                static_cast<float>(source) * alpha;
  return static_cast<std::uint8_t>(std::lround(std::max(0.0f, std::min(255.0f, value))));
}

void FillRect(ReferenceRenderer::Image* image, float x, float y, float width, float height,
              Rgba color) {
  int x0 = static_cast<int>(std::lround(x));
  int y0 = static_cast<int>(std::lround(y));
  int x1 = static_cast<int>(std::lround(x + width));
  int y1 = static_cast<int>(std::lround(y + height));
  x0 = std::max(0, x0);
  y0 = std::max(0, y0);
  x1 = std::min(image->width, x1);
  y1 = std::min(image->height, y1);
  const float alpha = static_cast<float>(color.a) / 255.0f;
  for (int py = y0; py < y1; ++py) {
    for (int px = x0; px < x1; ++px) {
      Rgba destination = image->At(px, py);
      Rgba blended;
      blended.r = BlendChannel(destination.r, color.r, alpha);
      blended.g = BlendChannel(destination.g, color.g, alpha);
      blended.b = BlendChannel(destination.b, color.b, alpha);
      blended.a = 255;
      image->Set(px, py, blended);
    }
  }
}

void DrawGlyph(ReferenceRenderer::Image* image, const GlyphQuad& quad, const GlyphAtlas& atlas) {
  if (quad.page < 0 || static_cast<std::size_t>(quad.page) >= atlas.pages().size()) return;
  const AtlasPage& page = atlas.pages()[static_cast<std::size_t>(quad.page)];
  const int page_size = atlas.page_size();
  const int source_x = static_cast<int>(std::lround(quad.u0 * static_cast<float>(page_size)));
  const int source_y = static_cast<int>(std::lround(quad.v0 * static_cast<float>(page_size)));
  const int glyph_width = static_cast<int>(std::lround(quad.width));
  const int glyph_height = static_cast<int>(std::lround(quad.height));
  const int destination_x = static_cast<int>(std::lround(quad.x));
  const int destination_y = static_cast<int>(std::lround(quad.y));

  for (int row = 0; row < glyph_height; ++row) {
    for (int column = 0; column < glyph_width; ++column) {
      int sx = source_x + column;
      int sy = source_y + row;
      if (sx < 0 || sy < 0 || sx >= page_size || sy >= page_size) continue;
      std::uint8_t coverage =
          page.pixels[static_cast<std::size_t>(sy) * static_cast<std::size_t>(page_size) +
                      static_cast<std::size_t>(sx)];
      if (coverage == 0) continue;
      int px = destination_x + column;
      int py = destination_y + row;
      if (px < 0 || py < 0 || px >= image->width || py >= image->height) continue;
      float alpha = (static_cast<float>(coverage) / 255.0f) *
                    (static_cast<float>(quad.color.a) / 255.0f);
      Rgba destination = image->At(px, py);
      Rgba blended;
      blended.r = BlendChannel(destination.r, quad.color.r, alpha);
      blended.g = BlendChannel(destination.g, quad.color.g, alpha);
      blended.b = BlendChannel(destination.b, quad.color.b, alpha);
      blended.a = 255;
      image->Set(px, py, blended);
    }
  }
}

}  // namespace

Rgba ReferenceRenderer::Image::At(int x, int y) const {
  Rgba color;
  if (x < 0 || y < 0 || x >= width || y >= height) return color;
  std::size_t index = (static_cast<std::size_t>(y) * static_cast<std::size_t>(width) +
                       static_cast<std::size_t>(x)) *
                      4;
  color.r = pixels[index];
  color.g = pixels[index + 1];
  color.b = pixels[index + 2];
  color.a = pixels[index + 3];
  return color;
}

void ReferenceRenderer::Image::Set(int x, int y, Rgba color) {
  if (x < 0 || y < 0 || x >= width || y >= height) return;
  std::size_t index = (static_cast<std::size_t>(y) * static_cast<std::size_t>(width) +
                       static_cast<std::size_t>(x)) *
                      4;
  pixels[index] = color.r;
  pixels[index + 1] = color.g;
  pixels[index + 2] = color.b;
  pixels[index + 3] = color.a;
}

ReferenceRenderer::Image ReferenceRenderer::Render(const RenderFrame& frame,
                                                   const GlyphAtlas& atlas, int width,
                                                   int height) {
  Image image;
  image.width = std::max(1, width);
  image.height = std::max(1, height);
  image.pixels.assign(static_cast<std::size_t>(image.width) *
                          static_cast<std::size_t>(image.height) * 4,
                      0);
  for (int y = 0; y < image.height; ++y) {
    for (int x = 0; x < image.width; ++x) image.Set(x, y, frame.background);
  }

  // Same layer order as the GL backend (spec §10.1).
  for (const Quad& quad : frame.backgrounds) {
    FillRect(&image, quad.x, quad.y, quad.width, quad.height, quad.color);
  }
  for (const GlyphQuad& quad : frame.glyphs) DrawGlyph(&image, quad, atlas);
  for (const Quad& quad : frame.decorations) {
    FillRect(&image, quad.x, quad.y, quad.width, quad.height, quad.color);
  }
  for (const Quad& quad : frame.cursor) {
    FillRect(&image, quad.x, quad.y, quad.width, quad.height, quad.color);
  }
  for (const GlyphQuad& quad : frame.cursor_glyphs) DrawGlyph(&image, quad, atlas);
  return image;
}

bool ReferenceRenderer::WritePpm(const Image& image, const std::string& path) {
  std::FILE* file = std::fopen(path.c_str(), "wb");
  if (file == nullptr) return false;
  std::fprintf(file, "P6\n%d %d\n255\n", image.width, image.height);
  for (int y = 0; y < image.height; ++y) {
    for (int x = 0; x < image.width; ++x) {
      Rgba color = image.At(x, y);
      std::fputc(color.r, file);
      std::fputc(color.g, file);
      std::fputc(color.b, file);
    }
  }
  std::fclose(file);
  return true;
}

std::string ReferenceRenderer::Fingerprint(const Image& image) {
  Bytes digest = crypto::Sha256(ByteView(image.pixels.data(), image.pixels.size()));
  return HexEncode(ByteView(digest));
}

}  // namespace render
}  // namespace tmirror
