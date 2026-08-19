#include "tm/render/view.h"

#include <cmath>

namespace tmirror {
namespace render {

void Viewport::SetSurfaceSize(float width, float height) {
  if (width == surface_width_ && height == surface_height_) return;
  surface_width_ = width;
  surface_height_ = height;
  // A rotation changes what "fits" means, so a view that was still following the
  // content refits rather than keeping a scale chosen for the old screen.
  if (follows_content_) {
    Fit();
  } else {
    Clamp();
  }
}

void Viewport::SetContentSize(float width, float height) {
  const bool changed = width != content_width_ || height != content_height_;
  content_width_ = width;
  content_height_ = height;
  if (!changed) return;
  if (follows_content_) {
    Fit();
  } else {
    Clamp();
  }
}

void Viewport::Fit() {
  follows_content_ = true;
  if (content_width_ <= 0.0f || surface_width_ <= 0.0f) return;

  // Fit the width: a terminal is read left to right, so hidden columns cost more than
  // small text does.
  transform_.scale = surface_width_ / content_width_;
  if (transform_.scale < kMinScale) transform_.scale = kMinScale;
  if (transform_.scale > kMaxScale) transform_.scale = kMaxScale;
  transform_.offset_x = 0.0f;

  // Anchor to the *bottom* when the grid is taller than the screen. A terminal's last
  // row is where the prompt and cursor are: showing the top of a 50-row grid and
  // hiding the line being typed on would be exactly backwards. Clamp() centres it
  // instead when it fits, so this only applies when there is something to scroll.
  const float scaled_height = content_height_ * transform_.scale;
  transform_.offset_y =
      scaled_height > surface_height_ ? surface_height_ - scaled_height : 0.0f;
  Clamp();
}

void Viewport::ZoomBy(float factor, float focus_x, float focus_y) {
  if (!(factor > 0.0f)) return;
  const float before = transform_.scale;
  float after = before * factor;
  if (after < kMinScale) after = kMinScale;
  if (after > kMaxScale) after = kMaxScale;
  if (after == before) return;

  // Keep the point under the fingers where it is; without this the terminal slides
  // away from whatever the user is looking at as they zoom.
  transform_.offset_x = focus_x - (focus_x - transform_.offset_x) * (after / before);
  transform_.offset_y = focus_y - (focus_y - transform_.offset_y) * (after / before);
  transform_.scale = after;
  follows_content_ = false;
  // A pinch is the user taking the view; it ends both kinds of following. The early
  // return above is what keeps the constant scale factor of 1 that a resting two-finger
  // gesture produces from counting as one.
  follow_output_ = false;
  Clamp();
}

void Viewport::PanBy(float dx, float dy) {
  if (std::fabs(dx) < kMinPanPixels && std::fabs(dy) < kMinPanPixels) return;
  const float before_x = transform_.offset_x;
  const float before_y = transform_.offset_y;
  transform_.offset_x += dx;
  transform_.offset_y += dy;
  Clamp();
  // A drag the clamp swallowed whole did not move the picture, so it did not take the
  // view from the content or from the output either. Without this, dragging at an edge
  // that cannot move silently turns following off.
  if (transform_.offset_x == before_x && transform_.offset_y == before_y) return;
  follows_content_ = false;
  follow_output_ = false;
}

bool Viewport::RevealContentRows(float top, float height, float margin) {
  if (transform_.scale <= 0.0f || surface_height_ <= 0.0f || !(height > 0.0f)) {
    return false;
  }
  const float before = transform_.offset_y;

  // Where the band currently sits on the surface, and the strip of surface it should
  // end up inside.
  const float band_top = top * transform_.scale + transform_.offset_y;
  const float band_height = height * transform_.scale;
  const float visible_top = margin;
  const float visible_bottom = surface_height_ - margin;

  if (band_height <= visible_bottom - visible_top) {
    if (band_top + band_height > visible_bottom) {
      transform_.offset_y -= band_top + band_height - visible_bottom;
    } else if (band_top < visible_top) {
      transform_.offset_y += visible_top - band_top;
    }
  }
  // The bounds still win: following may not push the view off the terminal, and on an
  // axis that cannot pan it stays centred.
  Clamp();
  return transform_.offset_y != before;
}

void Viewport::Clamp() {
  if (transform_.scale < kMinScale) transform_.scale = kMinScale;
  if (transform_.scale > kMaxScale) transform_.scale = kMaxScale;
  if (content_width_ <= 0.0f || content_height_ <= 0.0f) return;

  const float scaled_width = content_width_ * transform_.scale;
  const float scaled_height = content_height_ * transform_.scale;

  // On an axis where the content is smaller than the screen, centre it. Otherwise keep
  // it covering the screen, so no edge can be dragged into empty space.
  if (scaled_width <= surface_width_) {
    transform_.offset_x = (surface_width_ - scaled_width) * 0.5f;
  } else {
    if (transform_.offset_x > 0.0f) transform_.offset_x = 0.0f;
    if (transform_.offset_x < surface_width_ - scaled_width) {
      transform_.offset_x = surface_width_ - scaled_width;
    }
  }
  if (scaled_height <= surface_height_) {
    transform_.offset_y = (surface_height_ - scaled_height) * 0.5f;
  } else {
    if (transform_.offset_y > 0.0f) transform_.offset_y = 0.0f;
    if (transform_.offset_y < surface_height_ - scaled_height) {
      transform_.offset_y = surface_height_ - scaled_height;
    }
  }
}

bool Viewport::SurfaceToContent(float x, float y, float* out_x, float* out_y) const {
  if (transform_.scale <= 0.0f || content_width_ <= 0.0f) return false;
  const float content_x = (x - transform_.offset_x) / transform_.scale;
  const float content_y = (y - transform_.offset_y) / transform_.scale;
  if (out_x != nullptr) *out_x = content_x;
  if (out_y != nullptr) *out_y = content_y;
  return content_x >= 0.0f && content_y >= 0.0f && content_x < content_width_ &&
         content_y < content_height_;
}

}  // namespace render
}  // namespace tmirror
