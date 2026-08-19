#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "tm/util/bytes.h"

namespace tmirror {

std::string ToLowerAscii(const std::string& s);
bool EqualsIgnoreCaseAscii(const std::string& a, const std::string& b);
bool StartsWith(const std::string& s, const std::string& prefix);
bool EndsWith(const std::string& s, const std::string& suffix);
std::string Trim(const std::string& s);
std::vector<std::string> Split(const std::string& s, char delimiter);
std::string Join(const std::vector<std::string>& parts, const std::string& sep);
std::string HexEncode(ByteView bytes);
bool HexDecode(const std::string& hex, Bytes* out);

/// Percent-encode for a URL path or query component.
std::string UrlEncode(const std::string& s);

/// Parse a non-negative decimal integer with an explicit bound. Returns false on
/// overflow, trailing junk or an empty string: server-provided numbers are untrusted
/// input (spec §12).
bool ParseUint64(const std::string& s, std::uint64_t max, std::uint64_t* out);

/// Format helper used in place of iostreams, which are heavyweight on Android.
std::string Concat(std::initializer_list<std::string> parts);
std::string Uint64ToString(std::uint64_t v);
std::string Int64ToString(std::int64_t v);

/// Status::ToString and log lines must never carry payload, so this truncates and
/// escapes anything that came off the wire before it can reach a message string.
std::string SanitizeForMessage(const std::string& s, std::size_t max_length = 120);

}  // namespace tmirror
