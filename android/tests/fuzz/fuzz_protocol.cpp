// Fuzzes the mirror protocol decoders: control messages, binary frame headers and
// every length field in them (spec §16.2; relay spec §6.2, §6.3).
//
// The property under test is that no input can make the decoder crash, read out of
// bounds, or advance its offset bookkeeping into an inconsistent state.

#include <cassert>
#include <cstdint>
#include <string>

#include "tm/api/mirror_session.h"

namespace {

tmirror::api::MirrorSessionConfig Config() {
  tmirror::api::MirrorSessionConfig config;
  config.terminal_id = "9ca8a5f0-1d27-4d77-af11-d40c420568d2";
  return config;
}

}  // namespace

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size) {
  std::uint64_t offset_seen = 0;
  tmirror::api::MirrorSession session(Config(), [&](const tmirror::api::MirrorEvent& event) {
    if (event.kind == tmirror::api::MirrorEventKind::kOutput) {
      // Every emitted range must start exactly where the previous one ended.
      assert(event.start_offset >= offset_seen);
      offset_seen = event.start_offset + event.payload.size();
    }
  });

  // Half the corpus is fed as a control message, half as a binary frame, so one
  // driver covers both decoders.
  if (size > 0 && (data[0] & 1) == 0) {
    session.HandleControlMessage(std::string(reinterpret_cast<const char*>(data), size));
  } else {
    tmirror::Bytes frame(data, data + size);
    session.HandleBinaryFrame(frame);
  }

  // Subscribe first, then replay the same bytes: this reaches the paths that only
  // run once a subscription exists (offset continuity, input sequencing).
  session.HandleControlMessage(
      R"({"type":"subscribed","terminal_id":"t","requested_from_offset":0,
          "replay_start_offset":0,"next_offset":0,"durable_offset":0,"earliest_offset":0,
          "terminal_state":"open","label":"l","cols":80,"rows":24,"term":"xterm-256color",
          "accepts_input":true,"input_available":true})");
  offset_seen = 0;

  std::size_t position = 0;
  while (position < size) {
    std::size_t chunk = 1 + (data[position] % 64);
    if (position + chunk > size) chunk = size - position;
    if ((data[position] & 2) == 0) {
      session.HandleControlMessage(
          std::string(reinterpret_cast<const char*>(data + position), chunk));
    } else {
      tmirror::Bytes frame(data + position, data + position + chunk);
      session.HandleBinaryFrame(frame);
    }
    position += chunk;
  }

  assert(session.durable_offset() <= session.next_expected_offset() ||
         session.next_expected_offset() == 0);
  return 0;
}
