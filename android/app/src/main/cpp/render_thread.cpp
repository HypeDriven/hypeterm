#include "render_thread.h"

#include "tm/render/follow.h"

#include "tm/util/log.h"

#include <android/log.h>
#include <android/native_window.h>

#include <chrono>

#include "tm/util/time.h"

namespace tmirror {
namespace android {
namespace {

constexpr const char kTag[] = "Hypeterm";
/// Cursor blink period halves (spec §10.1 allows redrawing for the blink alone).
constexpr Millis kBlinkHalfPeriodMs = 500;

}  // namespace

RenderThread::RenderThread(render::GlyphRasterizer* rasterizer, float font_size_px,
                           float density)
    : rasterizer_(rasterizer), font_size_px_(font_size_px), density_(density) {
  render::GlyphAtlas::Options options;
  options.page_size = 1024;
  options.max_pages = 4;
  atlas_ = std::make_unique<render::GlyphAtlas>(options, rasterizer_);
  metrics_ = rasterizer_ != nullptr ? rasterizer_->MeasureCell(font_size_px, density)
                                    : render::CellMetrics();
  metrics_dirty_ = false;
}

RenderThread::~RenderThread() { Stop(); }

void RenderThread::Start() {
  if (running_.exchange(true)) return;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    stopping_ = false;
  }
  thread_ = std::thread([this] { ThreadMain(); });
}

void RenderThread::Stop() {
  if (!running_.exchange(false)) return;
  {
    // Published under the lock the render thread holds while it evaluates its wait
    // predicate, exactly as every other wakeup here is. An atomic alone is not enough:
    // the predicate can read it as still running and then block *after* the notify has
    // already come and gone, and with the network thread stopped one line earlier
    // nothing will ever wake it again — the UI thread waits in join() below for a
    // thread that is asleep forever.
    std::lock_guard<std::mutex> lock(mutex_);
    stopping_ = true;
  }
  condition_.notify_all();
  if (thread_.joinable()) thread_.join();
}

void RenderThread::SetSurface(ANativeWindow* window, int width, int height) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    // Unconditionally, even when the pointer repeats: every ANativeWindow_fromSurface is
    // a reference of its own, so a pair of calls that coalesce before this thread wakes
    // must drop the first rather than assume the two are the same acquisition.
    if (pending_window_ != nullptr) ANativeWindow_release(pending_window_);
    pending_window_ = window;
    surface_width_ = width;
    surface_height_ = height;
    surface_changed_ = true;
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

void RenderThread::SetFontSize(float font_size_px, float density) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    font_size_px_ = font_size_px;
    density_ = density;
    metrics_dirty_ = true;
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

void RenderThread::SetColors(render::Rgba foreground, render::Rgba background,
                             float minimum_contrast) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    palette_.set_default_foreground(foreground);
    palette_.set_default_background(background);
    palette_.set_minimum_contrast(minimum_contrast);
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

void RenderThread::SetFocused(bool focused) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (focused_ == focused) return;
    focused_ = focused;
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

void RenderThread::SetSnapshot(term::SnapshotRef snapshot) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    snapshot_ = std::move(snapshot);
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

void RenderThread::RequestRedraw() {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    redraw_requested_ = true;
  }
  condition_.notify_all();
}

render::GridSize RenderThread::CurrentGrid() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return grid_;
}

render::CellMetrics RenderThread::metrics() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return metrics_;
}

void RenderThread::SetGridCallback(std::function<void(int, int)> callback) {
  std::lock_guard<std::mutex> lock(mutex_);
  grid_callback_ = std::move(callback);
}

void RenderThread::UpdateMetrics() {
  // Measuring calls into the JVM, so it happens outside the lock. The atlas is only
  // ever touched on this thread, so clearing it needs no lock either.
  bool dirty = false;
  float font_size = 0.0f;
  float density = 1.0f;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    dirty = metrics_dirty_;
    metrics_dirty_ = false;
    font_size = font_size_px_;
    density = density_;
  }
  if (!dirty || rasterizer_ == nullptr) return;

  render::CellMetrics measured = rasterizer_->MeasureCell(font_size, density);
  {
    std::lock_guard<std::mutex> lock(mutex_);
    metrics_ = measured;
  }
  // A different cell size makes every cached glyph the wrong size.
  atlas_->Clear();
}

void RenderThread::SetContentSizeLocked(float width, float height) {
  viewport_.SetSurfaceSize(static_cast<float>(surface_width_),
                           static_cast<float>(surface_height_));
  viewport_.SetContentSize(width, height);
}

void RenderThread::ZoomBy(float factor, float focus_x, float focus_y) {
  float scale = 0.0f;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    viewport_.ZoomBy(factor, focus_x, focus_y);
    scale = viewport_.scale();
  }
  // The gesture chain crosses three layers and cannot be driven from a shell — SELinux
  // refuses synthetic touch — so the one link a device test cannot reach says so
  // itself. Geometry only; nothing here came off the wire.
  TM_LOG_DEBUG(kTag, "zoom %.3f -> scale %.3f at (%.0f, %.0f)", factor, scale, focus_x,
               focus_y);
  RequestRedraw();
}

void RenderThread::PanBy(float dx, float dy) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    viewport_.PanBy(dx, dy);
  }
  RequestRedraw();
}

void RenderThread::ResetView() {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    viewport_.Fit();
    // The one gesture that always gets the user back to a sane view should get them
    // back to the live output too. Fit() itself stays geometry (see Viewport::Fit).
    viewport_.SetFollowOutput(true);
  }
  RequestRedraw();
}

void RenderThread::SetFollowOutput(bool follow) {
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (viewport_.follow_output() == follow) return;
    viewport_.SetFollowOutput(follow);
  }
  // A selection made on an idle terminal changes the state with nothing to redraw for,
  // and the host layer only learns of the change from a frame.
  RequestRedraw();
}

void RenderThread::SetFollowCallback(std::function<void(bool)> callback) {
  std::lock_guard<std::mutex> lock(mutex_);
  follow_callback_ = std::move(callback);
}

RenderThread::View RenderThread::view() const {
  std::lock_guard<std::mutex> lock(mutex_);
  const render::ViewTransform transform = viewport_.transform();
  View out;
  out.scale = transform.scale;
  out.offset_x = transform.offset_x;
  out.offset_y = transform.offset_y;
  out.content_width = viewport_.content_width();
  out.content_height = viewport_.content_height();
  return out;
}

bool RenderThread::SurfaceToTerminal(float x, float y, float* out_x, float* out_y) const {
  std::lock_guard<std::mutex> lock(mutex_);
  return viewport_.SurfaceToContent(x, y, out_x, out_y);
}

void RenderThread::ThreadMain() {
#if defined(TM_ENABLE_GLES)
  Status initialized = egl_.Initialize();
  if (!initialized.ok()) {
    __android_log_print(ANDROID_LOG_ERROR, kTag, "EGL initialisation failed");
  }
#endif

  bool blink_on = true;
  Millis last_blink = Clock::System()->MonotonicMillis();

  while (running_.load()) {
    bool needs_blink_timer = false;
    {
      std::unique_lock<std::mutex> lock(mutex_);
      if (!redraw_requested_ && !surface_changed_) {
        // Wait indefinitely unless a blinking cursor needs the next phase: an idle
        // terminal must not cost continuous GPU work (spec §10.1).
        needs_blink_timer = snapshot_ && snapshot_->cursor.visible &&
                            snapshot_->cursor.blinking && focused_;
        if (needs_blink_timer) {
          condition_.wait_for(lock, std::chrono::milliseconds(kBlinkHalfPeriodMs),
                              [this] { return redraw_requested_ || surface_changed_ ||
                                              stopping_; });
        } else {
          condition_.wait(lock, [this] {
            return redraw_requested_ || surface_changed_ || stopping_;
          });
        }
      }
      if (stopping_) break;

      if (surface_changed_) {
        surface_changed_ = false;
        ANativeWindow* window = pending_window_;
        pending_window_ = nullptr;
        int width = surface_width_;
        int height = surface_height_;
        lock.unlock();
#if defined(TM_ENABLE_GLES)
        Status status = egl_.SetWindow(window);
        if (window == nullptr) {
          // Surface loss stops rendering but keeps the terminal model (spec §11).
          renderer_.OnContextLost();
        } else if (status.ok()) {
          if (!renderer_.initialized()) renderer_.Initialize();
          renderer_.SetViewport(width, height);
        } else {
          __android_log_print(ANDROID_LOG_WARN, kTag, "surface binding failed");
        }
#else
        (void)width;
        (void)height;
#endif
        if (window != nullptr) ANativeWindow_release(window);
        lock.lock();
      }

      redraw_requested_ = false;
    }
    UpdateMetrics();

    Millis now = Clock::System()->MonotonicMillis();
    if (needs_blink_timer && now - last_blink >= kBlinkHalfPeriodMs) {
      blink_on = !blink_on;
      last_blink = now;
    }

    // Build and draw outside the lock; the snapshot is immutable so this is safe
    // while the parser keeps running.
    term::SnapshotRef snapshot;
    render::CellMetrics metrics;
    int width = 0;
    int height = 0;
    bool focused = true;
    {
      std::lock_guard<std::mutex> lock(mutex_);
      snapshot = snapshot_;
      metrics = metrics_;
      width = surface_width_;
      height = surface_height_;
      focused = focused_;
    }
    if (!snapshot || width <= 0 || height <= 0) continue;

    render::FrameOptions options;
    options.focused = focused;
    options.blink_on = blink_on;
    // The whole grid is drawn at its natural size from the origin. Nothing is fitted
    // to the screen here: the terminal is whatever size the publisher is running at,
    // and the view decides which part of it is on screen (spec §10.4).
    render::RenderFrame frame =
        render::BuildFrame(*snapshot, palette_, metrics, atlas_.get(), options);

    if (frame.needs_another_frame) {
      // Bounded rasterization, then ask for one more frame rather than stalling this
      // one on font work (spec §10.2).
      if (atlas_->ProcessPending(metrics) > 0) {
        frame = render::BuildFrame(*snapshot, palette_, metrics, atlas_.get(), options);
      }
      RequestRedraw();
    }

    // Fold the frame's natural size into the view before drawing, so a terminal that
    // resizes at the far end refits rather than leaving the user staring at blank space.
    render::ViewTransform transform;
    bool following = false;
    std::function<void(bool)> follow_callback;
    {
      std::lock_guard<std::mutex> lock(mutex_);
      SetContentSizeLocked(frame.width_px, frame.height_px);

      // Following is the user's standing intent *and* a session parked at the live
      // bottom. This thread is the only one that sees both: the intent is view state it
      // owns, and the other half arrives in the immutable snapshot (spec §6.2).
      following = viewport_.follow_output() && snapshot->following_output;
      if (following) {
        // A row of margin so the followed line is never flush against the edge.
        //
        // Deliberately inside the frame that is about to be presented, and with no
        // redraw request of its own: the reveal is idempotent, so asking for another
        // frame here would turn following into a busy loop on an idle terminal
        // (spec §10.1).
        const render::OutputAnchor anchor = render::AnchorForOutput(*snapshot, metrics);
        if (anchor.valid) {
          viewport_.RevealContentRows(anchor.top, anchor.height, metrics.cell_height);
        }
      }
      transform = viewport_.transform();
      if (following != follow_reported_) {
        follow_reported_ = following;
        follow_callback = follow_callback_;
      }
    }

#if defined(TM_ENABLE_GLES)
    if (egl_.has_surface()) {
      if (!renderer_.initialized()) renderer_.Initialize();
      renderer_.SetViewport(width, height);
      // Two passes: the terminal into a texture at its own size, then that texture
      // into the viewport under the view. Panning and zooming then cost one textured
      // quad rather than re-laying out every cell.
      Status rendered = renderer_.RenderToTexture(frame, *atlas_);
      if (rendered.ok()) {
        renderer_.Present(transform, frame.background);
      } else {
        // No offscreen target — an old or memory-starved GPU. Drawing straight to the
        // screen loses zoom, but showing the terminal beats showing nothing.
        renderer_.Draw(frame, *atlas_);
      }
      Status presented = egl_.SwapBuffers();
      if (!presented.ok() && egl_.context_lost()) {
        // Rebuild GPU resources; the terminal model is untouched.
        __android_log_print(ANDROID_LOG_WARN, kTag, "EGL context lost, rebuilding");
        renderer_.OnContextLost();
        atlas_->Clear();
        egl_.Initialize();
        RequestRedraw();
      }
    }
#endif

    // Still computed and reported, because a client with no publisher-reported size
    // has to render at *something* (spec §10.3). It no longer drives a resize
    // request: the publisher's size is the terminal's size.
    render::GridSize grid = render::ComputeGrid(static_cast<float>(width),
                                                static_cast<float>(height), metrics);
    std::function<void(int, int)> callback;
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (grid != grid_) {
        grid_ = grid;
        callback = grid_callback_;
      }
    }
    if (callback) callback(grid.columns, grid.rows);
    // After presenting, so the JNI hop cannot delay a frame. Only ever on a change.
    if (follow_callback) follow_callback(following);
  }

#if defined(TM_ENABLE_GLES)
  egl_.Shutdown();
#endif
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (pending_window_ != nullptr) {
      ANativeWindow_release(pending_window_);
      pending_window_ = nullptr;
    }
  }
}

}  // namespace android
}  // namespace tmirror
