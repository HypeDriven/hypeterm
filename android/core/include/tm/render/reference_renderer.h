#pragma once

#include <string>
#include <vector>

#include "tm/render/frame_builder.h"

namespace tmirror {
namespace render {

/// CPU renderer for a RenderFrame.
///
/// Golden-image tests need a result that does not depend on a GPU, a driver or an
/// installed font (spec §16.3). This draws the same layers in the same order as the
/// GL backend, so a layout regression shows up here first and without a device.
class ReferenceRenderer {
 public:
  struct Image {
    int width = 0;
    int height = 0;
    std::vector<std::uint8_t> pixels;  // RGBA8, row-major

    Rgba At(int x, int y) const;
    void Set(int x, int y, Rgba color);
    bool empty() const { return pixels.empty(); }
  };

  static Image Render(const RenderFrame& frame, const GlyphAtlas& atlas, int width, int height);

  /// Portable pixmap, the simplest format a human can open and a test can diff.
  static bool WritePpm(const Image& image, const std::string& path);
  /// Stable content hash for golden comparisons.
  static std::string Fingerprint(const Image& image);
};

}  // namespace render
}  // namespace tmirror
