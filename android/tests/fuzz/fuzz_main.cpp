// Deterministic corpus driver.
//
// Each fuzz target is a standard `LLVMFuzzerTestOneInput`, so the same source builds
// under libFuzzer when a clang toolchain is available (see README.md in this
// directory). This main() exists so the targets also run as ordinary CI tests with
// plain g++: it generates a reproducible corpus from a seeded PRNG and feeds it in.
//
// Corpus shapes are deliberately biased towards what actually breaks terminals:
// escape-sequence prefixes, control bytes and truncated UTF-8, mixed with random
// noise (spec §16.2).

#if !defined(TM_LIBFUZZER)

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "tm/util/random.h"

extern "C" int LLVMFuzzerTestOneInput(const std::uint8_t* data, std::size_t size);

namespace {

const char* const kInterestingFragments[] = {
    "\x1b[", "\x1b]", "\x1bP", "\x1b_", "\x1b^", "\x1bX", "\x1b#", "\x1b(", "\x1b)",
    "\x1b[?", "\x1b[>", "\x1b[=", "\x1b[38;5;", "\x1b[38;2;", "\x1b[4:3", "\x1b[1;2;3;4;5",
    "\x07", "\x1b\\", "\r\n", "\t", "\x08", "\x0e", "\x0f", "\x18", "\x1a",
    "\xE4\xB8\x80", "\xCC\x81", "\xF0\x9F\x98\x80", "\xC0\x80", "\xED\xA0\x80", "\xF5",
    "999999999", ";;;;;;;;", ":::::::", "0;", "?1049h", "?1049l", "?2004h", "2J", "H",
    "{\"type\":\"", "\"from_offset\":", "subscribed", "durable", "gap", "input.ack",
};
constexpr std::size_t kFragmentCount = sizeof(kInterestingFragments) / sizeof(char*);

std::vector<std::uint8_t> BuildCase(tmirror::Prng* prng, std::size_t max_size) {
  std::vector<std::uint8_t> data;
  std::size_t target = 1 + static_cast<std::size_t>(prng->Below(max_size));
  while (data.size() < target) {
    std::uint64_t choice = prng->Below(100);
    if (choice < 55) {
      const char* fragment = kInterestingFragments[prng->Below(kFragmentCount)];
      std::size_t length = std::strlen(fragment);
      data.insert(data.end(), fragment, fragment + length);
    } else if (choice < 80) {
      // Printable runs: the common case, and the one that must stay correct.
      std::size_t run = 1 + static_cast<std::size_t>(prng->Below(16));
      for (std::size_t i = 0; i < run; ++i) {
        data.push_back(static_cast<std::uint8_t>(0x20 + prng->Below(95)));
      }
    } else {
      data.push_back(static_cast<std::uint8_t>(prng->Below(256)));
    }
  }
  data.resize(target);
  return data;
}

}  // namespace

int main(int argc, char** argv) {
  std::size_t iterations = 5000;
  std::size_t max_size = 4096;
  std::uint64_t seed = 0x243F6A8885A308D3ULL;
  for (int i = 1; i < argc; ++i) {
    if (std::strncmp(argv[i], "--iterations=", 13) == 0) {
      iterations = static_cast<std::size_t>(std::strtoul(argv[i] + 13, nullptr, 10));
    } else if (std::strncmp(argv[i], "--max-size=", 11) == 0) {
      max_size = static_cast<std::size_t>(std::strtoul(argv[i] + 11, nullptr, 10));
    } else if (std::strncmp(argv[i], "--seed=", 7) == 0) {
      seed = std::strtoull(argv[i] + 7, nullptr, 10);
    } else if (std::strcmp(argv[i], "--help") == 0) {
      std::printf("usage: %s [--iterations=N] [--max-size=N] [--seed=N] [file...]\n", argv[0]);
      return 0;
    } else {
      // A file argument replays a saved reproducer.
      std::FILE* file = std::fopen(argv[i], "rb");
      if (file == nullptr) {
        std::fprintf(stderr, "cannot open %s\n", argv[i]);
        return 1;
      }
      std::vector<std::uint8_t> data;
      std::uint8_t buffer[4096];
      while (true) {
        std::size_t read = std::fread(buffer, 1, sizeof(buffer), file);
        if (read == 0) break;
        data.insert(data.end(), buffer, buffer + read);
      }
      std::fclose(file);
      LLVMFuzzerTestOneInput(data.data(), data.size());
      return 0;
    }
  }

  tmirror::Prng prng(seed);
  for (std::size_t i = 0; i < iterations; ++i) {
    std::vector<std::uint8_t> data = BuildCase(&prng, max_size);
    LLVMFuzzerTestOneInput(data.data(), data.size());
  }
  std::printf("%zu cases, seed %llu: no crash, no unbounded growth\n", iterations,
              static_cast<unsigned long long>(seed));
  return 0;
}

#endif  // !TM_LIBFUZZER
