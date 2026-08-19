#pragma once

#if defined(TM_ENABLE_GLES)

#include <cstdint>
#include <vector>

#include "tm/render/frame_builder.h"
#include "tm/render/view.h"
#include "tm/util/result.h"

struct ANativeWindow;

namespace tmirror {
namespace render {

/// Owns the EGL display, config, context and window surface (spec §10.1).
///
/// Every method must be called on the render thread. Losing the context is expected
/// on Android, so `Draw` reports it and the renderer rebuilds its GPU objects from
/// the terminal model, which lives elsewhere and is never lost with it.
class EglSurface {
 public:
  EglSurface();
  ~EglSurface();

  EglSurface(const EglSurface&) = delete;
  EglSurface& operator=(const EglSurface&) = delete;

  /// Creates the display and context. Safe to call again after a loss.
  Status Initialize();
  /// Binds a window. Passing nullptr releases the current surface but keeps the
  /// context, so terminal state and GPU objects survive a surface-only loss.
  Status SetWindow(ANativeWindow* window);
  Status MakeCurrent();
  /// Presents the frame. Returns kInternal with `context_lost()` set when the context
  /// was lost and must be rebuilt.
  Status SwapBuffers();
  void Shutdown();

  bool has_surface() const { return surface_ != nullptr; }
  bool context_lost() const { return context_lost_; }
  int width() const { return width_; }
  int height() const { return height_; }

 private:
  void* display_ = nullptr;
  void* context_ = nullptr;
  void* surface_ = nullptr;
  void* config_ = nullptr;
  ANativeWindow* window_ = nullptr;
  int width_ = 0;
  int height_ = 0;
  bool context_lost_ = false;
};

/// OpenGL ES 3.0 backend for a RenderFrame.
///
/// It holds no terminal state: everything it needs arrives in the frame, which is why
/// rebuilding after a context loss cannot lose anything (spec §10.1, acceptance
/// criterion 7).
class GlRenderer {
 public:
  GlRenderer();
  ~GlRenderer();

  /// Compiles programs and allocates buffers against the current context.
  Status Initialize();
  /// Forgets every GL handle without calling GL: used after a context loss, when the
  /// old names are already invalid.
  void OnContextLost();
  bool initialized() const { return initialized_; }

  void SetViewport(int width, int height);
  /// Draws straight to the bound framebuffer, filling the viewport.
  void Draw(const RenderFrame& frame, const GlyphAtlas& atlas);

  /// Renders the whole terminal into an offscreen texture at its natural size.
  ///
  /// Separated from presentation because the two change at different rates: the
  /// terminal is redrawn when its content changes, while a pinch or a drag only moves
  /// the view — and re-laying out a 200-column grid for every touch event would be
  /// wasted work at exactly the moment smoothness matters.
  Status RenderToTexture(const RenderFrame& frame, const GlyphAtlas& atlas);

  /// Draws the last rendered texture into the viewport under `view`.
  void Present(const ViewTransform& view, Rgba background);

  /// The offscreen texture's size, which is the grid's natural pixel size clamped to
  /// what the GPU will hold. Zero until `RenderToTexture` has succeeded.
  int texture_width() const { return target_width_; }
  int texture_height() const { return target_height_; }
  /// Terminal pixels per texture pixel. Below 1 when the grid was too large for the
  /// GPU and had to be rendered smaller; callers fold it into their own mapping.
  float texture_scale() const { return target_scale_; }

 private:
  bool EnsureAtlasTextures(const GlyphAtlas& atlas);
  bool EnsureTarget(int width, int height);
  void DrawSolidQuads(const std::vector<Quad>& quads);
  void DrawGlyphQuads(const std::vector<GlyphQuad>& quads, const GlyphAtlas& atlas);
  void DrawFrameLayers(const RenderFrame& frame, const GlyphAtlas& atlas);

  bool initialized_ = false;
  int viewport_width_ = 0;
  int viewport_height_ = 0;
  int target_width_ = 0;
  int target_height_ = 0;
  float target_scale_ = 1.0f;
  std::uint32_t target_framebuffer_ = 0;
  std::uint32_t target_texture_ = 0;
  std::uint32_t blit_program_ = 0;
  std::uint32_t blit_sampler_ = 0;
  std::uint32_t blit_vao_ = 0;

  std::uint32_t solid_program_ = 0;
  std::uint32_t glyph_program_ = 0;
  std::uint32_t solid_projection_ = 0;
  std::uint32_t glyph_projection_ = 0;
  std::uint32_t glyph_sampler_ = 0;
  std::uint32_t vertex_buffer_ = 0;
  std::uint32_t solid_vao_ = 0;
  std::uint32_t glyph_vao_ = 0;
  std::vector<std::uint32_t> atlas_textures_;
  std::uint64_t atlas_generation_ = 0;
  std::vector<float> scratch_;
};

}  // namespace render
}  // namespace tmirror

#endif  // TM_ENABLE_GLES
