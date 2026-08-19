#pragma once

#include <jni.h>

#include <mutex>
#include <string>

#include "tm/render/atlas.h"

namespace tmirror {
namespace android {

/// Glyph rasterization through `android.graphics` (spec §10.2).
///
/// The platform text stack is the right tool here: it brings system font fallback
/// and shaping, which is what makes combining marks and unusual scripts render at
/// all. This is one of the few places the specification allows a JVM bridge, and it
/// is called only from the render thread.
class AndroidRasterizer : public render::GlyphRasterizer {
 public:
  AndroidRasterizer(JavaVM* vm, jobject rasterizer);
  ~AndroidRasterizer() override;

  bool Rasterize(const render::GlyphKey& key, const render::CellMetrics& metrics,
                 render::GlyphBitmap* out) override;
  render::CellMetrics MeasureCell(float font_size_px, float density) override;

  /// Called when the font size changes so cached metrics are recomputed.
  void Invalidate();

 private:
  JNIEnv* AttachCurrentThread(bool* attached);

  JavaVM* vm_;
  jobject rasterizer_ = nullptr;
  jmethodID rasterize_method_ = nullptr;
  jmethodID measure_method_ = nullptr;
  std::mutex mutex_;
};

}  // namespace android
}  // namespace tmirror
