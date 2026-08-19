#include "tm/util/json.h"

#include <cmath>
#include <cstdio>
#include <cstdlib>

#include "tm/util/strings.h"

namespace tmirror {
namespace {
const std::string& EmptyString() {
  static const std::string kEmpty;
  return kEmpty;
}
}  // namespace

Json Json::Bool(bool v) {
  Json j;
  j.type_ = Type::kBool;
  j.bool_ = v;
  return j;
}

Json Json::Number(double v) {
  Json j;
  j.type_ = Type::kNumber;
  j.number_ = v;
  char buf[40];
  int n = std::snprintf(buf, sizeof(buf), "%.17g", v);
  j.literal_.assign(buf, static_cast<std::size_t>(n < 0 ? 0 : n));
  return j;
}

Json Json::Uint(std::uint64_t v) {
  Json j;
  j.type_ = Type::kNumber;
  j.number_ = static_cast<double>(v);
  j.literal_ = Uint64ToString(v);
  return j;
}

Json Json::Int(std::int64_t v) {
  Json j;
  j.type_ = Type::kNumber;
  j.number_ = static_cast<double>(v);
  j.literal_ = Int64ToString(v);
  return j;
}

Json Json::String(std::string v) {
  Json j;
  j.type_ = Type::kString;
  j.string_ = std::move(v);
  return j;
}

Json Json::Array() {
  Json j;
  j.type_ = Type::kArray;
  return j;
}

Json Json::Object() {
  Json j;
  j.type_ = Type::kObject;
  return j;
}

bool Json::bool_value(bool fallback) const { return is_bool() ? bool_ : fallback; }
double Json::double_value(double fallback) const { return is_number() ? number_ : fallback; }
const std::string& Json::string_value() const {
  return is_string() ? string_ : EmptyString();
}

bool Json::AsUint64(std::uint64_t* out) const {
  if (!is_number()) return false;
  return ParseUint64(literal_, UINT64_MAX, out);
}

bool Json::AsInt64(std::int64_t* out) const {
  if (!is_number()) return false;
  bool negative = !literal_.empty() && literal_[0] == '-';
  std::uint64_t magnitude = 0;
  std::string digits = negative ? literal_.substr(1) : literal_;
  if (!ParseUint64(digits, negative ? 9223372036854775808ULL : 9223372036854775807ULL,
                   &magnitude)) {
    return false;
  }
  *out = negative ? -static_cast<std::int64_t>(magnitude) : static_cast<std::int64_t>(magnitude);
  return true;
}

bool Json::AsUint32Bounded(std::uint32_t max, std::uint32_t* out) const {
  std::uint64_t v = 0;
  if (!AsUint64(&v)) return false;
  if (v > max) return false;
  *out = static_cast<std::uint32_t>(v);
  return true;
}

const Json* Json::Find(const std::string& key) const {
  if (!is_object()) return nullptr;
  for (const auto& member : object_) {
    if (member.first == key) return &member.second;
  }
  return nullptr;
}

std::string Json::GetString(const std::string& key, const std::string& fallback) const {
  const Json* v = Find(key);
  return (v != nullptr && v->is_string()) ? v->string_ : fallback;
}

bool Json::GetBool(const std::string& key, bool fallback) const {
  const Json* v = Find(key);
  return (v != nullptr && v->is_bool()) ? v->bool_ : fallback;
}

bool Json::GetUint64(const std::string& key, std::uint64_t* out) const {
  const Json* v = Find(key);
  return v != nullptr && v->AsUint64(out);
}

bool Json::GetOptionalBool(const std::string& key, bool* out) const {
  const Json* v = Find(key);
  if (v == nullptr || !v->is_bool()) return false;
  *out = v->bool_;
  return true;
}

void Json::Set(const std::string& key, Json value) {
  if (type_ != Type::kObject) {
    type_ = Type::kObject;
    object_.clear();
  }
  for (auto& member : object_) {
    if (member.first == key) {
      member.second = std::move(value);
      return;
    }
  }
  object_.emplace_back(key, std::move(value));
}

void Json::Append(Json value) {
  if (type_ != Type::kArray) {
    type_ = Type::kArray;
    array_.clear();
  }
  array_.push_back(std::move(value));
}

// ------------------------------------------------------------------------ parser

class JsonParser {
 public:
  JsonParser(const std::string& text, const Json::Limits& limits)
      : text_(text), limits_(limits) {}

  Result<Json> Run() {
    SkipWhitespace();
    Result<Json> value = ParseValue(0);
    if (!value.ok()) return value;
    SkipWhitespace();
    if (position_ != text_.size()) return Fail("trailing content after JSON value");
    return value;
  }

 private:
  Status Error(const std::string& message) const {
    return Status::Error(ErrorKind::kProtocolError, "json: " + message);
  }
  Result<Json> Fail(const std::string& message) const { return Error(message); }

  void SkipWhitespace() {
    while (position_ < text_.size()) {
      char c = text_[position_];
      if (c == ' ' || c == '\t' || c == '\n' || c == '\r') {
        ++position_;
      } else {
        break;
      }
    }
  }

  bool Consume(char c) {
    if (position_ < text_.size() && text_[position_] == c) {
      ++position_;
      return true;
    }
    return false;
  }

  Result<Json> ParseValue(std::size_t depth) {
    if (depth > limits_.max_depth) return Fail("nesting too deep");
    if (++elements_ > limits_.max_elements) return Fail("too many elements");
    if (position_ >= text_.size()) return Fail("unexpected end of input");

    char c = text_[position_];
    switch (c) {
      case '{': return ParseObject(depth);
      case '[': return ParseArray(depth);
      case '"': {
        std::string s;
        Status status = ParseString(&s);
        if (!status.ok()) return status;
        return Json::String(std::move(s));
      }
      case 't':
        if (text_.compare(position_, 4, "true") == 0) {
          position_ += 4;
          return Json::Bool(true);
        }
        return Fail("invalid literal");
      case 'f':
        if (text_.compare(position_, 5, "false") == 0) {
          position_ += 5;
          return Json::Bool(false);
        }
        return Fail("invalid literal");
      case 'n':
        if (text_.compare(position_, 4, "null") == 0) {
          position_ += 4;
          return Json::Null();
        }
        return Fail("invalid literal");
      default:
        return ParseNumber();
    }
  }

  Result<Json> ParseObject(std::size_t depth) {
    ++position_;  // '{'
    Json object = Json::Object();
    SkipWhitespace();
    if (Consume('}')) return object;
    while (true) {
      SkipWhitespace();
      if (position_ >= text_.size() || text_[position_] != '"') {
        return Fail("expected a member name");
      }
      std::string key;
      Status status = ParseString(&key);
      if (!status.ok()) return status;
      SkipWhitespace();
      if (!Consume(':')) return Fail("expected ':'");
      SkipWhitespace();
      Result<Json> value = ParseValue(depth + 1);
      if (!value.ok()) return value;
      object.Set(key, value.take());
      SkipWhitespace();
      if (Consume(',')) continue;
      if (Consume('}')) return object;
      return Fail("expected ',' or '}'");
    }
  }

  Result<Json> ParseArray(std::size_t depth) {
    ++position_;  // '['
    Json array = Json::Array();
    SkipWhitespace();
    if (Consume(']')) return array;
    while (true) {
      SkipWhitespace();
      Result<Json> value = ParseValue(depth + 1);
      if (!value.ok()) return value;
      array.Append(value.take());
      SkipWhitespace();
      if (Consume(',')) continue;
      if (Consume(']')) return array;
      return Fail("expected ',' or ']'");
    }
  }

  Status ParseString(std::string* out) {
    ++position_;  // opening quote
    out->clear();
    while (true) {
      if (position_ >= text_.size()) return Error("unterminated string");
      unsigned char c = static_cast<unsigned char>(text_[position_]);
      if (c == '"') {
        ++position_;
        return Status::Ok();
      }
      if (c == '\\') {
        ++position_;
        if (position_ >= text_.size()) return Error("unterminated escape");
        char e = text_[position_++];
        switch (e) {
          case '"': out->push_back('"'); break;
          case '\\': out->push_back('\\'); break;
          case '/': out->push_back('/'); break;
          case 'b': out->push_back('\b'); break;
          case 'f': out->push_back('\f'); break;
          case 'n': out->push_back('\n'); break;
          case 'r': out->push_back('\r'); break;
          case 't': out->push_back('\t'); break;
          case 'u': {
            std::uint32_t code = 0;
            Status status = ParseHex4(&code);
            if (!status.ok()) return status;
            if (code >= 0xD800 && code <= 0xDBFF) {
              // A high surrogate must be followed by its pair, otherwise the text is
              // replaced rather than rejected: control messages carry user labels.
              if (position_ + 1 < text_.size() && text_[position_] == '\\' &&
                  text_[position_ + 1] == 'u') {
                std::size_t saved = position_;
                position_ += 2;
                std::uint32_t low = 0;
                if (ParseHex4(&low).ok() && low >= 0xDC00 && low <= 0xDFFF) {
                  code = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                } else {
                  position_ = saved;
                  code = 0xFFFD;
                }
              } else {
                code = 0xFFFD;
              }
            } else if (code >= 0xDC00 && code <= 0xDFFF) {
              code = 0xFFFD;
            }
            AppendUtf8(code, out);
            break;
          }
          default:
            return Error("invalid escape");
        }
        continue;
      }
      if (c < 0x20) return Error("unescaped control character in string");
      out->push_back(text_[position_++]);
      if (out->size() > limits_.max_bytes) return Error("string too long");
    }
  }

  Status ParseHex4(std::uint32_t* out) {
    if (position_ + 4 > text_.size()) return Error("truncated \\u escape");
    std::uint32_t value = 0;
    for (int i = 0; i < 4; ++i) {
      char c = text_[position_++];
      int digit;
      if (c >= '0' && c <= '9') digit = c - '0';
      else if (c >= 'a' && c <= 'f') digit = c - 'a' + 10;
      else if (c >= 'A' && c <= 'F') digit = c - 'A' + 10;
      else return Error("invalid \\u escape");
      value = (value << 4) | static_cast<std::uint32_t>(digit);
    }
    *out = value;
    return Status::Ok();
  }

  static void AppendUtf8(std::uint32_t code, std::string* out) {
    if (code < 0x80) {
      out->push_back(static_cast<char>(code));
    } else if (code < 0x800) {
      out->push_back(static_cast<char>(0xC0 | (code >> 6)));
      out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else if (code < 0x10000) {
      out->push_back(static_cast<char>(0xE0 | (code >> 12)));
      out->push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
      out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
    } else {
      out->push_back(static_cast<char>(0xF0 | (code >> 18)));
      out->push_back(static_cast<char>(0x80 | ((code >> 12) & 0x3F)));
      out->push_back(static_cast<char>(0x80 | ((code >> 6) & 0x3F)));
      out->push_back(static_cast<char>(0x80 | (code & 0x3F)));
    }
  }

  Result<Json> ParseNumber() {
    std::size_t start = position_;
    if (position_ < text_.size() && (text_[position_] == '-' || text_[position_] == '+')) {
      if (text_[position_] == '+') return Fail("leading '+' is not valid JSON");
      ++position_;
    }
    std::size_t integer_start = position_;
    std::size_t digits = 0;
    while (position_ < text_.size() && text_[position_] >= '0' && text_[position_] <= '9') {
      ++position_;
      ++digits;
    }
    if (digits == 0) return Fail("expected a number");
    // JSON forbids leading zeros, and accepting them would make two encodings of the
    // same offset compare unequal.
    if (digits > 1 && text_[integer_start] == '0') return Fail("leading zero in a number");
    bool integral = true;
    if (position_ < text_.size() && text_[position_] == '.') {
      integral = false;
      ++position_;
      std::size_t fraction_digits = 0;
      while (position_ < text_.size() && text_[position_] >= '0' && text_[position_] <= '9') {
        ++position_;
        ++fraction_digits;
      }
      if (fraction_digits == 0) return Fail("expected digits after '.'");
    }
    if (position_ < text_.size() && (text_[position_] == 'e' || text_[position_] == 'E')) {
      integral = false;
      ++position_;
      if (position_ < text_.size() && (text_[position_] == '+' || text_[position_] == '-')) {
        ++position_;
      }
      std::size_t exponent_digits = 0;
      while (position_ < text_.size() && text_[position_] >= '0' && text_[position_] <= '9') {
        ++position_;
        ++exponent_digits;
      }
      if (exponent_digits == 0) return Fail("expected digits in exponent");
    }

    std::string literal = text_.substr(start, position_ - start);
    Json value;
    value.type_ = Json::Type::kNumber;
    value.literal_ = literal;
    value.number_ = std::strtod(literal.c_str(), nullptr);
    (void)integral;
    return value;
  }

  const std::string& text_;
  const Json::Limits& limits_;
  std::size_t position_ = 0;
  std::size_t elements_ = 0;
};

Result<Json> Json::Parse(const std::string& text, const Limits& limits) {
  if (text.size() > limits.max_bytes) {
    return Status::Error(ErrorKind::kProtocolError, "json: document exceeds size limit");
  }
  JsonParser parser(text, limits);
  return parser.Run();
}

// -------------------------------------------------------------------- serializer

namespace {

void SerializeString(const std::string& s, std::string* out) {
  out->push_back('"');
  for (char raw : s) {
    unsigned char c = static_cast<unsigned char>(raw);
    switch (c) {
      case '"': *out += "\\\""; break;
      case '\\': *out += "\\\\"; break;
      case '\b': *out += "\\b"; break;
      case '\f': *out += "\\f"; break;
      case '\n': *out += "\\n"; break;
      case '\r': *out += "\\r"; break;
      case '\t': *out += "\\t"; break;
      default:
        if (c < 0x20) {
          char buf[8];
          std::snprintf(buf, sizeof(buf), "\\u%04x", c);
          *out += buf;
        } else {
          out->push_back(static_cast<char>(c));
        }
    }
  }
  out->push_back('"');
}

void SerializeValue(const Json& value, std::string* out) {
  switch (value.type()) {
    case Json::Type::kNull: *out += "null"; return;
    case Json::Type::kBool: *out += value.bool_value() ? "true" : "false"; return;
    case Json::Type::kNumber: {
      std::uint64_t u = 0;
      std::int64_t i = 0;
      if (value.AsUint64(&u)) {
        *out += Uint64ToString(u);
      } else if (value.AsInt64(&i)) {
        *out += Int64ToString(i);
      } else {
        char buf[40];
        int n = std::snprintf(buf, sizeof(buf), "%.17g", value.double_value());
        out->append(buf, static_cast<std::size_t>(n < 0 ? 0 : n));
      }
      return;
    }
    case Json::Type::kString: SerializeString(value.string_value(), out); return;
    case Json::Type::kArray: {
      out->push_back('[');
      bool first = true;
      for (const Json& item : value.items()) {
        if (!first) out->push_back(',');
        first = false;
        SerializeValue(item, out);
      }
      out->push_back(']');
      return;
    }
    case Json::Type::kObject: {
      out->push_back('{');
      bool first = true;
      for (const auto& member : value.members()) {
        if (!first) out->push_back(',');
        first = false;
        SerializeString(member.first, out);
        out->push_back(':');
        SerializeValue(member.second, out);
      }
      out->push_back('}');
      return;
    }
  }
}

}  // namespace

std::string Json::Serialize() const {
  std::string out;
  SerializeValue(*this, &out);
  return out;
}

}  // namespace tmirror
