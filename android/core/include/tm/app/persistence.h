#pragma once

#include <map>
#include <mutex>
#include <string>
#include <vector>

#include "tm/util/result.h"
#include "tm/util/time.h"

namespace tmirror {
namespace app {

/// Non-secret preferences and bounded cached metadata (spec §6.1).
///
/// Credentials never come here — they go to the Keystore-backed SecureStore. What
/// does live here is the persistent resume cursor per terminal, which the client may
/// only advance when the relay raises `durable_offset` (relay spec §6.2), plus UI
/// preferences such as font size.
class Preferences {
 public:
  /// Number of terminals whose resume cursor is retained. Bounded because a long-
  /// lived install would otherwise accumulate one entry per terminal ever seen.
  static constexpr std::size_t kMaxResumeEntries = 64;

  explicit Preferences(std::string path) : path_(std::move(path)) {}

  Status Load();
  Status Save();

  std::string GetString(const std::string& key, const std::string& fallback = "") const;
  void SetString(const std::string& key, const std::string& value);
  std::int64_t GetInt(const std::string& key, std::int64_t fallback) const;
  void SetInt(const std::string& key, std::int64_t value);
  bool GetBool(const std::string& key, bool fallback) const;
  void SetBool(const std::string& key, bool value);

  /// Durable resume cursor for a terminal: the offset of the next byte to request
  /// after a cold start.
  bool GetResumeOffset(const std::string& terminal_id, std::uint64_t* offset) const;
  void SetResumeOffset(const std::string& terminal_id, std::uint64_t offset, Millis now_unix_ms);
  void ForgetTerminal(const std::string& terminal_id);

  void Clear();
  bool dirty() const { return dirty_; }

 private:
  struct ResumeEntry {
    std::uint64_t offset = 0;
    Millis updated_unix_ms = 0;
  };

  void TrimResumeEntries();

  mutable std::mutex mutex_;
  std::string path_;
  std::map<std::string, std::string> values_;
  std::map<std::string, ResumeEntry> resume_;
  bool dirty_ = false;
};

}  // namespace app
}  // namespace tmirror
