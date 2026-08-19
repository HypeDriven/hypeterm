// Fuzzes the JSON parser, which sees every control message the relay sends
// (spec §7.4, §16.2).

#include <cassert>
#include <cstdint>
#include <string>

#include "tm/util/json.h"

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size) {
  std::string text(reinterpret_cast<const char*>(data), size);

  tmirror::Json::Limits limits;
  limits.max_bytes = 1 << 16;
  limits.max_depth = 16;
  limits.max_elements = 2000;

  tmirror::Result<tmirror::Json> parsed = tmirror::Json::Parse(text, limits);
  if (!parsed.ok()) return 0;

  // Anything that parses must serialise and re-parse to the same shape: a value that
  // survives one round trip but not the next would mean the decoder and the encoder
  // disagree, which is where protocol bugs hide.
  std::string serialized = parsed.value().Serialize();
  tmirror::Result<tmirror::Json> again = tmirror::Json::Parse(serialized, limits);
  assert(again.ok());
  assert(again.value().Serialize() == serialized);

  // Accessors must be safe on any parsed value.
  std::uint64_t number = 0;
  parsed.value().GetUint64("anything", &number);
  parsed.value().GetString("anything");
  parsed.value().GetBool("anything", false);
  return 0;
}
