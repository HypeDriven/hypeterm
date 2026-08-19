#pragma once

#include <cstddef>
#include <string>
#include <vector>

namespace tmirror {
namespace input {

/// Paste handling (spec §9.3).
///
/// Three requirements shape this: UTF-8 is preserved, line endings are normalised to
/// what a PTY expects from Enter, and a large paste is chunked so it neither blocks
/// the UI thread nor overruns the outbound queue. Bracketed paste, when the remote
/// enabled it, wraps the whole paste — the markers are never split across chunks in a
/// way that could leave the remote stuck in paste mode.
class Paste {
 public:
  struct Options {
    std::size_t chunk_bytes = 4096;
    /// Hard ceiling; anything beyond is refused rather than silently truncated.
    std::size_t max_bytes = 1024 * 1024;
    bool bracketed = false;
    /// Strip control characters other than tab and newline when bracketed paste is
    /// *not* active. Without the brackets a pasted escape sequence would be
    /// interpreted by the remote application, which is a known injection route.
    bool strip_controls_when_unbracketed = true;
  };

  static constexpr const char* kBracketStart = "\x1b[200~";
  static constexpr const char* kBracketEnd = "\x1b[201~";

  /// Normalise and split. Returns an empty vector when the input is empty or larger
  /// than `max_bytes` (`*too_large` says which).
  static std::vector<std::string> Prepare(const std::string& utf8_text, const Options& options,
                                          bool* too_large);

  /// Normalisation only, exposed for tests.
  static std::string Normalize(const std::string& utf8_text, const Options& options);
};

}  // namespace input
}  // namespace tmirror
