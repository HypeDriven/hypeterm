#pragma once

namespace tmirror {
namespace render {

/// How the terminal's own pixel space is placed in the viewport (spec §10.4).
struct ViewTransform {
  /// Terminal pixels to viewport pixels. Below 1 the whole grid is legible at a
  /// glance; at 1 the glyphs are at their rasterized size and crisp.
  float scale = 1.0f;
  /// Where the terminal's top-left corner sits in the viewport, in viewport pixels.
  float offset_x = 0.0f;
  float offset_y = 0.0f;
};

/// The window onto a terminal that is larger than the screen (spec §10.4).
///
/// The client does not resize the remote terminal. A phone asking a 200x50 desktop
/// session to become 55x24 would reflow it at the far end, where somebody is working;
/// so the terminal keeps whatever size its publisher runs at, is drawn once at that
/// size, and this decides which part of it is on screen.
///
/// Pure arithmetic with no GL and no platform: the awkward parts — keeping the view
/// inside the content, zooming about a point, refitting when the far end resizes —
/// are exactly the parts worth testing without a device.
class Viewport {
 public:
  /// Below this the text stops resolving; above it the offscreen texture is being
  /// magnified past its rasterized detail and there is nothing further to see.
  static constexpr float kMinScale = 0.15f;
  static constexpr float kMaxScale = 6.0f;

  /// Movement below this is not a pan. `ScaleGestureDetector` reports a focus point
  /// averaged from two raw touches, so a pinch delivers a stream of sub-pixel drags
  /// even while the fingers rest; treating those as a gesture would take the view away
  /// from the content for something the user did not do.
  static constexpr float kMinPanPixels = 0.5f;

  void SetSurfaceSize(float width, float height);

  /// The terminal's natural size. A change refits the view unless the user has taken
  /// control of it, in which case the view is only brought back inside the new bounds.
  void SetContentSize(float width, float height);

  /// Multiplies the zoom about a point on the surface, keeping what is under that
  /// point where it is.
  void ZoomBy(float factor, float focus_x, float focus_y);

  /// Moves the view by a drag, in surface pixels.
  void PanBy(float dx, float dy);

  /// Fits the terminal's width to the surface and marks the view as following the
  /// content again. A grid taller than the screen is anchored to its *bottom*, where the
  /// prompt and cursor are; one that fits is centred by Clamp().
  ///
  /// Geometry only: it deliberately does not re-arm output following, because it is
  /// also called from `SetSurfaceSize` and `SetContentSize`, and a rotation or a
  /// far-end resize must not resurrect a mode the user turned off.
  void Fit();

  /// Whether the view keeps the newest output on screen as it arrives (spec §5.2).
  ///
  /// Distinct from `follows_content()`, which is about *scale* — refit when the far
  /// end resizes — and which `Fit()` turns back on by itself. This one is the user's
  /// standing intent to watch the live output, so only the user turns it back on.
  void SetFollowOutput(bool follow) { follow_output_ = follow; }
  bool follow_output() const { return follow_output_; }

  /// Brings a band of terminal rows into view, moving as little as it can and never
  /// changing the zoom. Returns true when the view actually moved.
  ///
  /// Vertical only: the newest output arrives at the bottom, not at the right, and
  /// chasing the cursor's column would slide the text sideways on every wrapped line.
  /// A band taller than the visible region is left alone — there is no part of it that
  /// is more right to show than another, and moving would only fight the user.
  bool RevealContentRows(float top, float height, float margin);

  ViewTransform transform() const { return transform_; }
  float scale() const { return transform_.scale; }
  float content_width() const { return content_width_; }
  float content_height() const { return content_height_; }
  /// False once the user has zoomed or panned; a far-end resize then no longer moves
  /// the view out from under them.
  bool follows_content() const { return follows_content_; }

  /// Maps a point on the surface into the terminal's pixel space. Returns false when
  /// the point falls outside the terminal, which is a real answer: a tap in the margin
  /// should do nothing rather than land on the nearest cell.
  bool SurfaceToContent(float x, float y, float* out_x, float* out_y) const;

 private:
  void Clamp();

  ViewTransform transform_;
  float surface_width_ = 0.0f;
  float surface_height_ = 0.0f;
  float content_width_ = 0.0f;
  float content_height_ = 0.0f;
  bool follows_content_ = true;
  bool follow_output_ = true;
};

}  // namespace render
}  // namespace tmirror
