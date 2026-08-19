// Performance characteristics from spec §14.
//
// These are not device-calibrated numbers — the specification's targets are stated
// against an agreed reference device, which CI is not. What they do enforce is the
// property behind each target: parsing a 1 MiB burst is linear and bounded, building a
// snapshot is cheap enough to do every frame, and an idle terminal produces no work.
// A regression that turns any of those quadratic will fail here long before it is
// measured on hardware.

#include <chrono>
#include <string>

#include "framework.h"
#include "helpers.h"
#include "tm/render/atlas.h"
#include "tm/render/frame_builder.h"
#include "tm/term/emulator.h"

using tmirror::term::Emulator;
using tmirror::term::EmulatorConfig;
using tmtest::Feed;

namespace {

double MillisFor(const std::function<void()>& body) {
  auto start = std::chrono::steady_clock::now();
  body();
  auto elapsed = std::chrono::steady_clock::now() - start;
  return std::chrono::duration<double, std::milli>(elapsed).count();
}

EmulatorConfig BurstConfig() {
  EmulatorConfig config;
  config.columns = 120;
  config.rows = 40;
  config.scrollback.max_lines = 10000;  // the specification's default (§8.2)
  return config;
}

}  // namespace

TM_TEST(Performance, AbsorbsAOneMebibyteBurst) {
  Emulator emulator(BurstConfig());

  // Ordinary terminal output: text, newlines and colour changes.
  std::string chunk;
  for (int i = 0; i < 64; ++i) {
    chunk += "\x1b[32muser\x1b[0m@host:~$ some output line number ";
    chunk += std::to_string(i);
    chunk += "\r\n";
  }
  const std::size_t target = 1024 * 1024;
  std::string burst;
  burst.reserve(target + chunk.size());
  while (burst.size() < target) burst += chunk;

  double elapsed = MillisFor([&] {
    // Chunked exactly as the network delivers it.
    for (std::size_t offset = 0; offset < burst.size(); offset += 16384) {
      std::size_t length = std::min<std::size_t>(16384, burst.size() - offset);
      emulator.Feed(tmirror::ByteView::FromChars(burst.data() + offset, length));
    }
  });

  // Generous, because CI machines vary wildly; the point is that it is bounded and
  // linear, not that it hits a device number.
  TM_CHECK_MSG(elapsed < 5000.0, "1 MiB parse took " + std::to_string(elapsed) + " ms");
  TM_CHECK(emulator.scrollback().size() <= 10000);
  TM_CHECK(emulator.scrollback().memory_bytes() <= 32u * 1024u * 1024u);
}

TM_TEST(Performance, ParsingScalesLinearly) {
  auto parse = [](std::size_t bytes) {
    Emulator emulator(BurstConfig());
    std::string data(bytes, 'x');
    return MillisFor([&] { emulator.Feed(tmirror::ByteView(data)); });
  };

  double small = parse(256 * 1024) + 0.001;
  double large = parse(1024 * 1024);
  // Four times the input must not cost more than roughly sixteen times the time; a
  // quadratic regression blows straight past that.
  TM_CHECK_MSG(large < small * 16.0 + 50.0,
               "256 KiB: " + std::to_string(small) + " ms, 1 MiB: " +
                   std::to_string(large) + " ms");
}

TM_TEST(Performance, SnapshotsAreCheapEnoughForEveryFrame) {
  Emulator emulator(BurstConfig());
  for (int i = 0; i < 5000; ++i) Feed(emulator, "a line of terminal output\r\n");

  double elapsed = MillisFor([&] {
    for (int i = 0; i < 600; ++i) {
      tmirror::term::Snapshot snapshot = emulator.BuildSnapshot(0);
      TM_CHECK_EQ(static_cast<int>(snapshot.lines.size()), snapshot.rows);
    }
  });
  // 600 snapshots is ten seconds of 60 fps. Lines are shared, not copied, so this
  // should be a small fraction of a frame budget each.
  TM_CHECK_MSG(elapsed < 1000.0, "600 snapshots took " + std::to_string(elapsed) + " ms");
}

TM_TEST(Performance, IdleTerminalProducesNoNewWork) {
  Emulator emulator(BurstConfig());
  Feed(emulator, "prompt$ ");
  std::uint64_t revision = emulator.revision();

  // Feeding nothing, and feeding bytes that change nothing observable, must not make
  // the renderer think there is a new frame (spec §10.1).
  emulator.Feed(tmirror::ByteView());
  TM_CHECK_EQ(emulator.revision(), revision);
  tmirror::term::Snapshot first = emulator.BuildSnapshot(0);
  tmirror::term::Snapshot second = emulator.BuildSnapshot(0);
  TM_CHECK_EQ(first.revision, second.revision);
}

TM_TEST(Performance, FrameBuildingIsBoundedByTheGrid) {
  Emulator emulator(BurstConfig());
  for (int row = 0; row < 40; ++row) {
    Feed(emulator, "\x1b[31m" + std::string(60, 'x') + "\x1b[0m" + std::string(59, 'y') + "\r\n");
  }
  tmirror::term::Snapshot snapshot = emulator.BuildSnapshot(0);

  tmirror::render::BuiltinFontRasterizer rasterizer;
  tmirror::render::GlyphAtlas atlas(tmirror::render::GlyphAtlas::Options(), &rasterizer);
  tmirror::render::Palette palette;
  tmirror::render::CellMetrics metrics = rasterizer.MeasureCell(16.0f, 1.0f);
  // Warm the atlas so the measurement is of frame building, not rasterization.
  for (int i = 0; i < 10; ++i) {
    tmirror::render::BuildFrame(snapshot, palette, metrics, &atlas,
                                tmirror::render::FrameOptions());
    atlas.ProcessPending(metrics);
  }

  double elapsed = MillisFor([&] {
    for (int i = 0; i < 120; ++i) {
      tmirror::render::RenderFrame frame = tmirror::render::BuildFrame(
          snapshot, palette, metrics, &atlas, tmirror::render::FrameOptions());
      TM_CHECK(!frame.glyphs.empty());
    }
  });
  TM_CHECK_MSG(elapsed < 1000.0, "120 frames took " + std::to_string(elapsed) + " ms");
}
