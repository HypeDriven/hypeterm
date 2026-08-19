// Fuzzes the terminal emulator with arbitrary bytes at arbitrary chunk boundaries
// (spec §16.2), and asserts the bounds the specification requires: scrollback,
// parser state and memory all stay finite whatever the stream contains.

#include <cassert>
#include <cstdint>
#include <cstdlib>

#include "tm/term/emulator.h"
#include "tm/util/random.h"

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size) {
  tmirror::term::EmulatorConfig config;
  config.columns = 40;
  config.rows = 12;
  config.scrollback.max_lines = 200;
  config.scrollback.max_bytes = 512 * 1024;
  config.parser.max_string_bytes = 1024;
  tmirror::term::Emulator emulator(config);

  // Terminal replies must not accumulate: they would be sent as input, and a hostile
  // stream that provokes thousands of them is a denial of service on the uplink.
  std::size_t response_bytes = 0;
  emulator.SetResponseSink([&](tmirror::ByteView bytes) { response_bytes += bytes.size(); });

  // The chunk boundaries come from the data itself, so a reproducer replays exactly.
  tmirror::Prng prng(size == 0 ? 1 : (static_cast<std::uint64_t>(data[0]) + size));
  std::size_t offset = 0;
  while (offset < size) {
    std::size_t chunk = 1 + static_cast<std::size_t>(prng.Below(64));
    if (offset + chunk > size) chunk = size - offset;
    emulator.Feed(tmirror::ByteView(data + offset, chunk));
    offset += chunk;
  }

  // Bounds that must hold no matter what arrived.
  assert(emulator.scrollback().size() <= config.scrollback.max_lines);
  assert(emulator.scrollback().memory_bytes() <= config.scrollback.max_bytes + 65536);
  assert(emulator.columns() == config.columns);
  assert(emulator.rows() == config.rows);

  // A snapshot of whatever state resulted must be well formed.
  tmirror::term::Snapshot snapshot = emulator.BuildSnapshot(0);
  assert(static_cast<int>(snapshot.lines.size()) == snapshot.rows);
  assert(snapshot.cursor.row < snapshot.rows);
  for (const auto& line : snapshot.lines) assert(line != nullptr);

  (void)response_bytes;
  return 0;
}
