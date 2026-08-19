#include "tm/app/persistence.h"

#include <cstdio>

#include "tm/util/json.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace app {
namespace {

Result<std::string> ReadFile(const std::string& path, std::size_t max_bytes) {
  std::FILE* file = std::fopen(path.c_str(), "rb");
  if (file == nullptr) {
    return Status::Error(ErrorKind::kNotFound, "preferences file is not present");
  }
  std::string contents;
  char buffer[4096];
  while (true) {
    std::size_t read = std::fread(buffer, 1, sizeof(buffer), file);
    if (read == 0) break;
    if (contents.size() + read > max_bytes) {
      std::fclose(file);
      return Status::Error(ErrorKind::kStorageError, "preferences file is too large");
    }
    contents.append(buffer, read);
  }
  std::fclose(file);
  return contents;
}

Status WriteFileAtomically(const std::string& path, const std::string& contents) {
  std::string temporary = path + ".tmp";
  std::FILE* file = std::fopen(temporary.c_str(), "wb");
  if (file == nullptr) {
    return Status::Error(ErrorKind::kStorageError, "cannot open preferences for writing");
  }
  std::size_t written = std::fwrite(contents.data(), 1, contents.size(), file);
  int flushed = std::fflush(file);
  std::fclose(file);
  if (written != contents.size() || flushed != 0) {
    std::remove(temporary.c_str());
    return Status::Error(ErrorKind::kStorageError, "cannot write preferences");
  }
  if (std::rename(temporary.c_str(), path.c_str()) != 0) {
    std::remove(temporary.c_str());
    return Status::Error(ErrorKind::kStorageError, "cannot replace preferences");
  }
  return Status::Ok();
}

}  // namespace

Status Preferences::Load() {
  std::lock_guard<std::mutex> lock(mutex_);
  values_.clear();
  resume_.clear();
  dirty_ = false;

  Result<std::string> contents = ReadFile(path_, 256 * 1024);
  if (!contents.ok()) {
    // A missing file is the normal first-run case, not an error.
    return contents.status().kind() == ErrorKind::kNotFound ? Status::Ok() : contents.status();
  }
  Result<Json> parsed = Json::Parse(contents.value());
  if (!parsed.ok() || !parsed.value().is_object()) {
    return Status::Error(ErrorKind::kStorageError, "preferences are unreadable");
  }

  const Json* values = parsed.value().Find("values");
  if (values != nullptr && values->is_object()) {
    for (const auto& member : values->members()) {
      if (member.second.is_string()) values_[member.first] = member.second.string_value();
    }
  }
  const Json* resume = parsed.value().Find("resume");
  if (resume != nullptr && resume->is_object()) {
    for (const auto& member : resume->members()) {
      if (!member.second.is_object()) continue;
      ResumeEntry entry;
      member.second.GetUint64("offset", &entry.offset);
      std::uint64_t updated = 0;
      member.second.GetUint64("updated_unix_ms", &updated);
      entry.updated_unix_ms = static_cast<Millis>(updated);
      resume_[member.first] = entry;
    }
  }
  TrimResumeEntries();
  return Status::Ok();
}

Status Preferences::Save() {
  std::lock_guard<std::mutex> lock(mutex_);
  Json values = Json::Object();
  for (const auto& entry : values_) values.Set(entry.first, Json::String(entry.second));

  Json resume = Json::Object();
  for (const auto& entry : resume_) {
    Json record = Json::Object();
    record.Set("offset", Json::Uint(entry.second.offset));
    record.Set("updated_unix_ms", Json::Uint(static_cast<std::uint64_t>(entry.second.updated_unix_ms)));
    resume.Set(entry.first, std::move(record));
  }

  Json root = Json::Object();
  root.Set("version", Json::Uint(1));
  root.Set("values", std::move(values));
  root.Set("resume", std::move(resume));

  Status status = WriteFileAtomically(path_, root.Serialize());
  if (status.ok()) dirty_ = false;
  return status;
}

std::string Preferences::GetString(const std::string& key, const std::string& fallback) const {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = values_.find(key);
  return it == values_.end() ? fallback : it->second;
}

void Preferences::SetString(const std::string& key, const std::string& value) {
  std::lock_guard<std::mutex> lock(mutex_);
  values_[key] = value;
  dirty_ = true;
}

std::int64_t Preferences::GetInt(const std::string& key, std::int64_t fallback) const {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = values_.find(key);
  if (it == values_.end()) return fallback;
  bool negative = !it->second.empty() && it->second[0] == '-';
  std::uint64_t magnitude = 0;
  std::string digits = negative ? it->second.substr(1) : it->second;
  if (!ParseUint64(digits, 9223372036854775807ULL, &magnitude)) return fallback;
  return negative ? -static_cast<std::int64_t>(magnitude) : static_cast<std::int64_t>(magnitude);
}

void Preferences::SetInt(const std::string& key, std::int64_t value) {
  SetString(key, Int64ToString(value));
}

bool Preferences::GetBool(const std::string& key, bool fallback) const {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = values_.find(key);
  if (it == values_.end()) return fallback;
  return it->second == "true" || it->second == "1";
}

void Preferences::SetBool(const std::string& key, bool value) {
  SetString(key, value ? "true" : "false");
}

bool Preferences::GetResumeOffset(const std::string& terminal_id, std::uint64_t* offset) const {
  std::lock_guard<std::mutex> lock(mutex_);
  auto it = resume_.find(terminal_id);
  if (it == resume_.end()) return false;
  *offset = it->second.offset;
  return true;
}

void Preferences::SetResumeOffset(const std::string& terminal_id, std::uint64_t offset,
                                  Millis now_unix_ms) {
  std::lock_guard<std::mutex> lock(mutex_);
  ResumeEntry& entry = resume_[terminal_id];
  // Offsets never move backwards for a terminal (relay spec §7.2).
  if (offset > entry.offset) entry.offset = offset;
  entry.updated_unix_ms = now_unix_ms;
  dirty_ = true;
  TrimResumeEntries();
}

void Preferences::ForgetTerminal(const std::string& terminal_id) {
  std::lock_guard<std::mutex> lock(mutex_);
  if (resume_.erase(terminal_id) > 0) dirty_ = true;
}

void Preferences::TrimResumeEntries() {
  while (resume_.size() > kMaxResumeEntries) {
    auto oldest = resume_.begin();
    for (auto it = resume_.begin(); it != resume_.end(); ++it) {
      if (it->second.updated_unix_ms < oldest->second.updated_unix_ms) oldest = it;
    }
    resume_.erase(oldest);
    dirty_ = true;
  }
}

void Preferences::Clear() {
  std::lock_guard<std::mutex> lock(mutex_);
  values_.clear();
  resume_.clear();
  dirty_ = true;
}

}  // namespace app
}  // namespace tmirror
