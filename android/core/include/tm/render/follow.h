#pragma once

#include "tm/render/metrics.h"
#include "tm/term/snapshot.h"

namespace tmirror {
namespace render {

/// The band of the terminal's own pixel space that a view following the output should
/// be showing (spec §5.2, §10.4).
///
/// Vertical only: the newest output arrives at the bottom, not at the right, and
/// chasing the cursor's column would slide the text sideways on every wrapped line and
/// every prompt.
struct OutputAnchor {
  float top = 0.0f;
  float height = 0.0f;
  /// False when there is nothing to chase and the right move is stillness.
  bool valid = false;
};

/// Where the newest output is landing.
///
/// Lives here rather than in the render thread because it is a policy with three real
/// cases and no GL or platform in any of them, which makes it exactly the kind of thing
/// worth testing without a device.
OutputAnchor AnchorForOutput(const term::Snapshot& snapshot, const CellMetrics& metrics);

}  // namespace render
}  // namespace tmirror
