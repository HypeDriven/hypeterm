// The window onto a terminal larger than the screen (spec §10.4).
//
// The client renders whatever size the publisher is running at and moves a view over
// it, rather than asking the far end to shrink. These cover the arithmetic that makes
// that usable: fitting, clamping, zooming about a point, and what happens when the
// terminal changes size underneath the user.

#include "../framework.h"
#include "tm/render/follow.h"
#include "tm/render/view.h"

using tmirror::render::Viewport;

namespace {

/// A phone-shaped surface and a desktop-shaped terminal: 200x50 cells at 10x20 pixels.
Viewport DesktopOnPhone() {
  Viewport viewport;
  viewport.SetSurfaceSize(1000.0f, 2000.0f);
  viewport.SetContentSize(2000.0f, 1000.0f);
  return viewport;
}

}  // namespace

TM_TEST(View, FittingShowsEveryColumn) {
  Viewport viewport = DesktopOnPhone();
  // Width is what gets fitted: a terminal is read left to right, so hidden columns
  // cost more than small text.
  TM_CHECK_NEAR(viewport.scale(), 0.5f, 1e-5f);
  TM_CHECK_NEAR(viewport.transform().offset_x, 0.0f, 1e-5f);

  // The whole grid is 500 pixels tall at that scale, so it is centred rather than
  // pinned to the top of a much taller screen.
  TM_CHECK_NEAR(viewport.transform().offset_y, (2000.0f - 500.0f) * 0.5f, 1e-3f);
}

TM_TEST(View, FittingShowsTheBottomWhenTheGridIsTallerThanTheScreen) {
  Viewport viewport;
  // A landscape phone and a 200x50 terminal: fitting the width leaves the grid taller
  // than the screen.
  viewport.SetSurfaceSize(1560.0f, 620.0f);
  viewport.SetContentSize(2000.0f, 1000.0f);

  const float scaled_height = viewport.content_height() * viewport.scale();
  TM_REQUIRE(scaled_height > 620.0f);
  // The last row is where the prompt and cursor are; showing the top of the grid and
  // hiding the line being typed on would be exactly backwards.
  TM_CHECK_NEAR(viewport.transform().offset_y, 620.0f - scaled_height, 0.5f);
}

TM_TEST(View, ZoomingKeepsThePointUnderTheFingers) {
  Viewport viewport = DesktopOnPhone();
  // Zoom in until the terminal overflows the screen on *both* axes. Focus is only
  // preserved on an axis that can pan: where the content is smaller than the screen it
  // is centred instead, and centring necessarily wins.
  viewport.ZoomBy(6.0f, 500.0f, 1000.0f);
  TM_REQUIRE(viewport.content_height() * viewport.scale() > 2000.0f);

  const float focus_x = 400.0f;
  const float focus_y = 900.0f;
  float before_x = 0.0f;
  float before_y = 0.0f;
  viewport.SurfaceToContent(focus_x, focus_y, &before_x, &before_y);

  viewport.ZoomBy(1.5f, focus_x, focus_y);

  float after_x = 0.0f;
  float after_y = 0.0f;
  viewport.SurfaceToContent(focus_x, focus_y, &after_x, &after_y);

  // Whatever character was under the pinch stays there; otherwise the terminal slides
  // away from what the user is looking at.
  TM_CHECK_NEAR(after_x, before_x, 0.5f);
  TM_CHECK_NEAR(after_y, before_y, 0.5f);
}

TM_TEST(View, CentringWinsOverFocusOnAnAxisThatCannotPan) {
  Viewport viewport = DesktopOnPhone();
  // 1000 tall at half scale is 500 on a 2000-tall screen: there is nowhere to pan
  // vertically, so the grid stays centred wherever the pinch happened.
  viewport.ZoomBy(2.0f, 0.0f, 0.0f);
  const float scaled_height = viewport.content_height() * viewport.scale();
  TM_REQUIRE(scaled_height < 2000.0f);
  TM_CHECK_NEAR(viewport.transform().offset_y, (2000.0f - scaled_height) * 0.5f, 0.5f);
}

TM_TEST(View, TheViewCannotBeDraggedOffTheContent) {
  Viewport viewport = DesktopOnPhone();
  viewport.ZoomBy(4.0f, 500.0f, 1000.0f);  // well past fitting, so panning is possible

  viewport.PanBy(100000.0f, 100000.0f);
  TM_CHECK(viewport.transform().offset_x <= 0.0f);
  TM_CHECK(viewport.transform().offset_y <= 0.0f);

  viewport.PanBy(-100000.0f, -100000.0f);
  const float scaled_width = viewport.content_width() * viewport.scale();
  const float scaled_height = viewport.content_height() * viewport.scale();
  // The far edge cannot be dragged past the far side of the screen: there is never
  // empty space where the terminal should be.
  TM_CHECK(viewport.transform().offset_x >= 1000.0f - scaled_width - 0.5f);
  TM_CHECK(viewport.transform().offset_y >= 2000.0f - scaled_height - 0.5f);
}

TM_TEST(View, ContentSmallerThanTheScreenIsCentredNotPinned) {
  Viewport viewport;
  viewport.SetSurfaceSize(1000.0f, 2000.0f);
  viewport.SetContentSize(400.0f, 200.0f);
  viewport.ZoomBy(0.5f, 500.0f, 1000.0f);

  const float scaled_width = viewport.content_width() * viewport.scale();
  TM_CHECK_NEAR(viewport.transform().offset_x, (1000.0f - scaled_width) * 0.5f, 0.5f);
}

TM_TEST(View, ZoomIsBounded) {
  Viewport viewport = DesktopOnPhone();
  for (int i = 0; i < 40; ++i) viewport.ZoomBy(2.0f, 500.0f, 1000.0f);
  TM_CHECK(viewport.scale() <= Viewport::kMaxScale);

  for (int i = 0; i < 80; ++i) viewport.ZoomBy(0.5f, 500.0f, 1000.0f);
  TM_CHECK(viewport.scale() >= Viewport::kMinScale);
}

TM_TEST(View, AResizeAtTheFarEndRefitsUntilTheUserTakesOver) {
  Viewport viewport = DesktopOnPhone();
  TM_CHECK(viewport.follows_content());

  // The publisher's window gets wider. Nobody has touched the view, so it refits and
  // the user still sees every column.
  viewport.SetContentSize(4000.0f, 1000.0f);
  TM_CHECK_NEAR(viewport.scale(), 0.25f, 1e-5f);

  // Once the user zooms, the view is theirs: a later resize must not yank it back.
  viewport.ZoomBy(4.0f, 500.0f, 1000.0f);
  TM_CHECK(!viewport.follows_content());
  const float chosen = viewport.scale();
  viewport.SetContentSize(3000.0f, 1200.0f);
  TM_CHECK_NEAR(viewport.scale(), chosen, 1e-5f);
  TM_CHECK(!viewport.follows_content());

  // ...but it is still brought back inside the new bounds.
  const float scaled_width = viewport.content_width() * viewport.scale();
  TM_CHECK(viewport.transform().offset_x >= 1000.0f - scaled_width - 0.5f);
}

TM_TEST(View, FittingAgainReturnsControlToTheContent) {
  Viewport viewport = DesktopOnPhone();
  viewport.ZoomBy(3.0f, 100.0f, 100.0f);
  TM_CHECK(!viewport.follows_content());

  viewport.Fit();
  TM_CHECK(viewport.follows_content());
  TM_CHECK_NEAR(viewport.scale(), 0.5f, 1e-5f);
}

TM_TEST(View, APointOutsideTheTerminalIsReportedAsOutside) {
  Viewport viewport = DesktopOnPhone();
  float x = 0.0f;
  float y = 0.0f;

  // Above the vertically centred grid: a tap there should do nothing rather than
  // land on the first row.
  TM_CHECK(!viewport.SurfaceToContent(500.0f, 10.0f, &x, &y));
  TM_CHECK(y < 0.0f);

  TM_CHECK(viewport.SurfaceToContent(500.0f, 1000.0f, &x, &y));
  TM_CHECK(x >= 0.0f && x < viewport.content_width());
  TM_CHECK(y >= 0.0f && y < viewport.content_height());
}

TM_TEST(View, RotationRefitsWhileTheViewStillFollows) {
  Viewport viewport = DesktopOnPhone();
  // Landscape: the same terminal, a wider screen, so it can be shown larger.
  viewport.SetSurfaceSize(2000.0f, 1000.0f);
  TM_CHECK_NEAR(viewport.scale(), 1.0f, 1e-5f);
}

TM_TEST(View, AZeroSizedContentIsNotDividedBy) {
  Viewport viewport;
  viewport.SetSurfaceSize(1000.0f, 2000.0f);
  viewport.SetContentSize(0.0f, 0.0f);
  viewport.Fit();
  viewport.ZoomBy(2.0f, 10.0f, 10.0f);
  viewport.PanBy(5.0f, 5.0f);
  // Nothing to map onto, and no crash or infinity on the way there.
  float x = 0.0f;
  TM_CHECK(!viewport.SurfaceToContent(1.0f, 1.0f, &x, nullptr));
  TM_CHECK(viewport.scale() > 0.0f);
}

// --------------------------------------------------------- following the output
//
// The view has two independent ways of being somewhere else: a scrollback position up
// in the history, and a window panned or zoomed away from where output arrives. These
// cover the second one — the arithmetic that keeps the newest row on screen, and the
// rule that a gesture ends it (spec §5.2).

namespace {

/// Zoomed in far enough that the 50-row grid is twice as tall as the screen, which is
/// the only situation where following has anywhere to move to. At the fitted scale of
/// 0.5, and even at 1.0, the whole grid is already on screen.
Viewport ZoomedPastTheScreen() {
  Viewport viewport = DesktopOnPhone();
  viewport.ZoomBy(8.0f, 0.0f, 0.0f);
  viewport.SetFollowOutput(true);
  return viewport;
}

/// Top of the last row of a 50-row grid, in the terminal's own pixels.
constexpr float kLastRowTop = 49.0f * 20.0f;
constexpr float kRowHeight = 20.0f;

}  // namespace

TM_TEST(View, AFittedViewFollowsTheOutputToStartWith) {
  Viewport viewport = DesktopOnPhone();
  TM_CHECK(viewport.follow_output());
}

TM_TEST(View, FollowingKeepsTheNewestRowOnScreenWhileZoomedIn) {
  // Zoomed in on the top-left, which is where a user reading a stack trace ends up.
  Viewport viewport = ZoomedPastTheScreen();
  float y = 0.0f;
  viewport.SurfaceToContent(500.0f, 1999.0f, nullptr, &y);
  TM_REQUIRE(y < kLastRowTop);  // the newest row really is off the bottom

  TM_CHECK(viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));

  const float top = kLastRowTop * viewport.scale() + viewport.transform().offset_y;
  TM_CHECK(top >= 0.0f);
  TM_CHECK(top + kRowHeight * viewport.scale() <= 2000.0f);
}

TM_TEST(View, FollowingNeverChangesTheZoomTheUserChose) {
  Viewport viewport = ZoomedPastTheScreen();
  const float chosen = viewport.scale();
  TM_REQUIRE(viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));
  // Re-zooming under somebody is hostile; Fit() is the gesture that changes scale.
  TM_CHECK_NEAR(viewport.scale(), chosen, 1e-5f);
}

TM_TEST(View, FollowingDoesNothingWhenTheRowIsAlreadyOnScreen) {
  Viewport viewport = ZoomedPastTheScreen();
  TM_REQUIRE(viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));
  const float settled = viewport.transform().offset_y;

  // Idempotence is what keeps following off the render thread's redraw path: a reveal
  // that reported movement every frame would be a busy loop on an idle terminal.
  TM_CHECK(!viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));
  TM_CHECK_NEAR(viewport.transform().offset_y, settled, 1e-5f);
}

TM_TEST(View, FollowingMovesBackUpWhenTheOutputJumpsToTheTop) {
  Viewport viewport = ZoomedPastTheScreen();
  TM_REQUIRE(viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));
  TM_REQUIRE(viewport.transform().offset_y < -1000.0f);  // parked at the bottom

  // `clear` puts the cursor back on the first row, and a run of output from there is
  // still the newest output. Following has to be able to move either way.
  TM_CHECK(viewport.RevealContentRows(0.0f, kRowHeight, kRowHeight));
  const float top = 0.0f * viewport.scale() + viewport.transform().offset_y;
  TM_CHECK(top >= 0.0f);
  TM_CHECK(top + kRowHeight * viewport.scale() <= 2000.0f);
}

TM_TEST(View, FollowingIsInertWhenTheWholeGridFitsTheScreen) {
  Viewport viewport = DesktopOnPhone();
  // Fitted: 1000 content pixels at half scale on a 2000-tall surface, so Clamp centres
  // it and there is nowhere for following to move it to.
  const float before = viewport.transform().offset_y;
  TM_CHECK(!viewport.RevealContentRows(kLastRowTop, kRowHeight, kRowHeight));
  TM_CHECK_NEAR(viewport.transform().offset_y, before, 1e-5f);
}

TM_TEST(View, ABandTallerThanTheScreenIsLeftWhereItIs) {
  Viewport viewport = ZoomedPastTheScreen();
  const float before = viewport.transform().offset_y;
  // No part of it is more right to show than another, so moving would only fight.
  TM_CHECK(!viewport.RevealContentRows(0.0f, viewport.content_height(), kRowHeight));
  TM_CHECK_NEAR(viewport.transform().offset_y, before, 1e-5f);
}

TM_TEST(View, FollowingCannotPushTheViewOffTheContent) {
  Viewport viewport = ZoomedPastTheScreen();
  // A row well past the end of the grid, which is what a stale cursor row would be.
  viewport.RevealContentRows(viewport.content_height() * 4.0f, kRowHeight, kRowHeight);
  const float scaled_height = viewport.content_height() * viewport.scale();
  TM_REQUIRE(scaled_height > 2000.0f);
  TM_CHECK(viewport.transform().offset_y <= 0.0f);
  TM_CHECK(viewport.transform().offset_y >= 2000.0f - scaled_height - 0.5f);
}

TM_TEST(View, PinchingStopsTheViewFollowingOutput) {
  Viewport viewport = DesktopOnPhone();
  viewport.ZoomBy(2.0f, 500.0f, 1000.0f);
  TM_CHECK(!viewport.follow_output());
}

TM_TEST(View, APinchThatChangesNothingKeepsFollowing) {
  Viewport viewport = DesktopOnPhone();
  // ScaleGestureDetector reports a factor of 1 continuously while two fingers rest;
  // that is not the user taking the view.
  viewport.ZoomBy(1.0f, 500.0f, 1000.0f);
  TM_CHECK(viewport.follow_output());

  // Nor is it at the zoom limit, where the scale cannot change any further.
  for (int i = 0; i < 40; ++i) viewport.ZoomBy(2.0f, 500.0f, 1000.0f);
  viewport.SetFollowOutput(true);
  viewport.ZoomBy(2.0f, 500.0f, 1000.0f);
  TM_CHECK(viewport.follow_output());
}

TM_TEST(View, DraggingStopsTheViewFollowingOutput) {
  Viewport viewport = ZoomedPastTheScreen();
  viewport.PanBy(0.0f, -60.0f);
  TM_CHECK(!viewport.follow_output());
  TM_CHECK(!viewport.follows_content());
}

TM_TEST(View, ASubPixelDragDoesNotTakeTheViewFromTheContent) {
  Viewport viewport = ZoomedPastTheScreen();
  const float before = viewport.transform().offset_y;

  // A pinch pans by the movement of a focus point averaged from two raw touches, so
  // resting fingers deliver a stream of these. They are not a gesture.
  for (int i = 0; i < 100; ++i) viewport.PanBy(0.2f, -0.3f);
  TM_CHECK(viewport.follow_output());
  TM_CHECK_NEAR(viewport.transform().offset_y, before, 1e-5f);
}

TM_TEST(View, ADragTheClampSwallowsDoesNotTakeTheView) {
  Viewport viewport = DesktopOnPhone();
  // Fitted, so the grid is centred vertically and there is nowhere to pan to.
  const float before = viewport.transform().offset_y;
  viewport.PanBy(0.0f, 500.0f);
  TM_CHECK_NEAR(viewport.transform().offset_y, before, 1e-5f);
  // Nothing moved, so nothing was taken: a drag at an edge must not silently stop the
  // view following the output.
  TM_CHECK(viewport.follow_output());
  TM_CHECK(viewport.follows_content());
}

TM_TEST(View, FittingIsGeometryAndDoesNotDecideWhetherToFollow) {
  Viewport viewport = DesktopOnPhone();
  viewport.SetFollowOutput(false);

  // Fit() runs from SetSurfaceSize and SetContentSize as well as from the user's
  // double tap, so re-arming here would resurrect the mode on every rotation and every
  // far-end resize.
  viewport.Fit();
  TM_CHECK(!viewport.follow_output());
  viewport.SetSurfaceSize(2000.0f, 1000.0f);
  TM_CHECK(!viewport.follow_output());
  viewport.SetContentSize(4000.0f, 1000.0f);
  TM_CHECK(!viewport.follow_output());
}

// --------------------------------------------------------- where the output lands
//
// Which row a following view should be showing. Three cases and no GL in any of them,
// which is why the policy lives in core rather than in the render thread (spec §5.2).

namespace {

tmirror::render::CellMetrics TenByTwenty() {
  tmirror::render::CellMetrics metrics;
  metrics.cell_width = 10.0f;
  metrics.cell_height = 20.0f;
  return metrics;
}

/// A snapshot shaped like one, without running an emulator: these assert the policy,
/// not the emulator's own bookkeeping.
tmirror::term::Snapshot GridSnapshot(int rows) {
  tmirror::term::Snapshot snapshot;
  snapshot.columns = 80;
  snapshot.rows = rows;
  snapshot.lines.resize(static_cast<std::size_t>(rows));
  return snapshot;
}

}  // namespace

TM_TEST(View, TheOutputAnchorIsTheCursorRow) {
  tmirror::term::Snapshot snapshot = GridSnapshot(50);
  snapshot.cursor.row = 30;
  snapshot.cursor.visible = true;

  tmirror::render::OutputAnchor anchor =
      tmirror::render::AnchorForOutput(snapshot, TenByTwenty());
  TM_CHECK(anchor.valid);
  TM_CHECK_NEAR(anchor.top, 600.0f, 1e-5f);
  TM_CHECK_NEAR(anchor.height, 20.0f, 1e-5f);
}

TM_TEST(View, AHiddenCursorOnAScrollingScreenAnchorsToTheLastRow) {
  tmirror::term::Snapshot snapshot = GridSnapshot(50);
  // A build log or a progress bar hides the caret and keeps scrolling; the last row is
  // still where its output arrives.
  snapshot.cursor.visible = false;
  snapshot.cursor.row = 3;

  tmirror::render::OutputAnchor anchor =
      tmirror::render::AnchorForOutput(snapshot, TenByTwenty());
  TM_CHECK(anchor.valid);
  TM_CHECK_NEAR(anchor.top, 49.0f * 20.0f, 1e-5f);
}

TM_TEST(View, AFullScreenApplicationWithNoCaretIsLeftAlone) {
  tmirror::term::Snapshot snapshot = GridSnapshot(50);
  snapshot.alt_screen = true;
  snapshot.cursor.visible = false;

  // A pager's canvas is the whole grid; there is no "latest output" row, and guessing
  // one would drag a zoomed-in reader to an arbitrary corner.
  TM_CHECK(!tmirror::render::AnchorForOutput(snapshot, TenByTwenty()).valid);
}

TM_TEST(View, AFullScreenApplicationWithACaretIsFollowedToIt) {
  tmirror::term::Snapshot snapshot = GridSnapshot(50);
  snapshot.alt_screen = true;
  snapshot.cursor.visible = true;
  snapshot.cursor.row = 12;

  // Keeping an editor's caret on screen at high zoom is the most useful thing this can
  // do on a phone.
  tmirror::render::OutputAnchor anchor =
      tmirror::render::AnchorForOutput(snapshot, TenByTwenty());
  TM_CHECK(anchor.valid);
  TM_CHECK_NEAR(anchor.top, 240.0f, 1e-5f);
}

TM_TEST(View, ACursorScrolledOutOfTheSnapshotIsNotChased) {
  tmirror::term::Snapshot snapshot = GridSnapshot(50);
  snapshot.alt_screen = true;
  snapshot.cursor.visible = true;
  // The emulator reports -1 when the scroll offset pushed the cursor out of view. On a
  // screen that scrolls the last row still answers; on the alternate screen nothing
  // does, and that is the window between an asked-for scroll and the frame that
  // reflects it.
  snapshot.cursor.row = -1;

  TM_CHECK(!tmirror::render::AnchorForOutput(snapshot, TenByTwenty()).valid);
}

TM_TEST(View, ThereIsNothingToFollowBeforeTheFirstFrame) {
  tmirror::term::Snapshot empty;
  TM_CHECK(!tmirror::render::AnchorForOutput(empty, TenByTwenty()).valid);

  tmirror::render::CellMetrics unmeasured;
  unmeasured.cell_height = 0.0f;
  TM_CHECK(!tmirror::render::AnchorForOutput(GridSnapshot(24), unmeasured).valid);
}
