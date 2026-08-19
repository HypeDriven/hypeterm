#pragma once

#include <atomic>
#include <condition_variable>
#include <functional>
#include <memory>
#include <mutex>
#include <thread>

#include "tm/render/atlas.h"
#include "tm/render/frame_builder.h"
#include "tm/render/gl_renderer.h"
#include "tm/render/metrics.h"
#include "tm/render/view.h"
#include "tm/term/snapshot.h"

struct ANativeWindow;

namespace tmirror {
namespace android {

/// The render thread (spec §6.2, §10.1).
///
/// It exclusively owns the EGL context and every GL call. It holds no terminal state:
/// what it draws is an immutable snapshot handed over by the controller, which is why
/// a context loss costs GPU objects and nothing else (spec §10.1, acceptance
/// criterion 7).
///
/// It redraws when the terminal changes, when the cursor blinks, when glyphs finish
/// rasterizing, or when the surface changes — and at no other time, so an idle
/// terminal costs no GPU work.
class RenderThread {
 public:
  RenderThread(render::GlyphRasterizer* rasterizer, float font_size_px, float density);
  ~RenderThread();

  void Start();
  void Stop();

  /// Ownership of `window` is taken; pass nullptr on surface destruction.
  void SetSurface(ANativeWindow* window, int width, int height);
  void SetFontSize(float font_size_px, float density);
  /// Default colours and the minimum contrast floor (spec §13).
  void SetColors(render::Rgba foreground, render::Rgba background, float minimum_contrast);
  void SetFocused(bool focused);
  void SetSnapshot(term::SnapshotRef snapshot);
  void RequestRedraw();

  /// Grid the current surface and cell metrics imply. Only a fallback now: the
  /// terminal's size is the publisher's (spec §10.3, §10.4). Returns {0,0} until a
  /// surface exists.
  render::GridSize CurrentGrid() const;
  render::CellMetrics metrics() const;

  /// Called whenever the computed grid changes, on the render thread.
  void SetGridCallback(std::function<void(int columns, int rows)> callback);

  // ------------------------------------------------------------------- the view
  //
  // The terminal is drawn once at the size the publisher is running at, and the view
  // decides which part of it the screen shows (spec §10.4). Clamping lives here
  // rather than in the gesture handler so there is one definition of where the view
  // may go, and so it stays right when the grid changes size underneath it.

  /// Multiplies the zoom about a focus point, in surface pixels.
  void ZoomBy(float factor, float focus_x, float focus_y);
  /// Moves the view by a drag, in surface pixels.
  void PanBy(float dx, float dy);
  /// Fits the terminal's width to the surface — anchoring to the bottom when the grid
  /// is taller than the screen — and starts following the newest output again.
  void ResetView();

  /// Whether the view keeps the newest output on screen (spec §5.2). Turned off by a
  /// gesture that takes the view — a pinch, a two-finger drag, a selection.
  void SetFollowOutput(bool follow);

  /// Called on the render thread whenever *effective* following changes: the user's
  /// intent above, combined with whether the session is parked at the live bottom.
  /// Edge-triggered, so an idle terminal reports nothing and there is no JNI work per
  /// frame.
  void SetFollowCallback(std::function<void(bool following)> callback);

  struct View {
    float scale = 1.0f;
    float offset_x = 0.0f;
    float offset_y = 0.0f;
    /// The terminal's natural size in pixels, or zero before the first frame.
    float content_width = 0.0f;
    float content_height = 0.0f;
  };
  View view() const;

  /// Maps a point on the surface to one in the terminal's own pixel space, which is
  /// what selection and mouse reporting need (spec §9.2).
  bool SurfaceToTerminal(float x, float y, float* out_x, float* out_y) const;

 private:
  void ThreadMain();
  void UpdateMetrics();
  /// Publishes the current surface and content sizes to the viewport. Caller holds
  /// the lock.
  void SetContentSizeLocked(float width, float height);

  render::GlyphRasterizer* rasterizer_;
  std::unique_ptr<render::GlyphAtlas> atlas_;
  render::Palette palette_;
  render::CellMetrics metrics_;
  float font_size_px_;
  float density_;

  std::thread thread_;
  std::atomic<bool> running_{false};
  /// Shutdown, published under `mutex_` because it is one of the conditions the render
  /// thread waits on. `running_` alone cannot be: see Stop().
  bool stopping_ = false;

  mutable std::mutex mutex_;
  std::condition_variable condition_;
  ANativeWindow* pending_window_ = nullptr;
  bool surface_changed_ = false;
  bool redraw_requested_ = false;
  bool metrics_dirty_ = true;
  bool focused_ = true;
  int surface_width_ = 0;
  int surface_height_ = 0;
  term::SnapshotRef snapshot_;
  render::GridSize grid_;
  std::function<void(int, int)> grid_callback_;
  std::function<void(bool)> follow_callback_;
  /// Last value handed to the host layer. Starts true because a fitted view follows.
  bool follow_reported_ = true;

  /// The window onto the terminal. All of its arithmetic lives in core/render so it
  /// can be tested without a device.
  render::Viewport viewport_;

#if defined(TM_ENABLE_GLES)
  render::EglSurface egl_;
  render::GlRenderer renderer_;
#endif
};

}  // namespace android
}  // namespace tmirror
