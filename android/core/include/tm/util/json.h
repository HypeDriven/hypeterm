#pragma once

#include <cstdint>
#include <map>
#include <memory>
#include <string>
#include <vector>

#include "tm/util/result.h"

namespace tmirror {

/// A deliberately small JSON implementation.
///
/// Every control message that reaches it came off a socket, so parsing is bounded in
/// three independent ways (spec §7.4, §12): total input size, nesting depth, and
/// element count. Numbers keep their literal text so a 64-bit byte offset survives
/// without passing through a double.
struct JsonLimits {
  std::size_t max_bytes = 1 << 20;  // 1 MiB; the relay's control messages are tiny
  std::size_t max_depth = 32;
  std::size_t max_elements = 20000;
};

class Json {
 public:
  enum class Type { kNull, kBool, kNumber, kString, kArray, kObject };
  using Limits = JsonLimits;

  Json() = default;
  static Json Null() { return Json(); }
  static Json Bool(bool v);
  static Json Number(double v);
  static Json Uint(std::uint64_t v);
  static Json Int(std::int64_t v);
  static Json String(std::string v);
  static Json Array();
  static Json Object();

  static Result<Json> Parse(const std::string& text, const Limits& limits = Limits());

  Type type() const { return type_; }
  bool is_null() const { return type_ == Type::kNull; }
  bool is_bool() const { return type_ == Type::kBool; }
  bool is_number() const { return type_ == Type::kNumber; }
  bool is_string() const { return type_ == Type::kString; }
  bool is_array() const { return type_ == Type::kArray; }
  bool is_object() const { return type_ == Type::kObject; }

  bool bool_value(bool fallback = false) const;
  double double_value(double fallback = 0.0) const;
  const std::string& string_value() const;

  /// Exact unsigned parse from the literal, so offsets above 2^53 stay exact.
  bool AsUint64(std::uint64_t* out) const;
  bool AsInt64(std::int64_t* out) const;
  /// Bounded unsigned accessor for untrusted dimensions and limits.
  bool AsUint32Bounded(std::uint32_t max, std::uint32_t* out) const;

  /// Object access. Returns nullptr when absent; unknown members are ignored by
  /// design (relay spec §12: ignore unknown fields).
  const Json* Find(const std::string& key) const;
  bool Has(const std::string& key) const { return Find(key) != nullptr; }
  std::string GetString(const std::string& key, const std::string& fallback = "") const;
  bool GetBool(const std::string& key, bool fallback) const;
  bool GetUint64(const std::string& key, std::uint64_t* out) const;
  bool GetOptionalBool(const std::string& key, bool* out) const;

  void Set(const std::string& key, Json value);
  void Append(Json value);
  const std::vector<Json>& items() const { return array_; }
  const std::vector<std::pair<std::string, Json>>& members() const { return object_; }

  std::string Serialize() const;

 private:
  Type type_ = Type::kNull;
  bool bool_ = false;
  double number_ = 0.0;
  std::string literal_;  // original number text, for exact integer recovery
  std::string string_;
  std::vector<Json> array_;
  std::vector<std::pair<std::string, Json>> object_;

  friend class JsonParser;
};

}  // namespace tmirror
