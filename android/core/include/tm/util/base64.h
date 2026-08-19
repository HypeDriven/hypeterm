#pragma once

#include <string>

#include "tm/util/bytes.h"

namespace tmirror {

/// Base64url without padding — the encoding the relay uses for keys, challenges,
/// signatures and tickets (relay spec §3.1, §5.1).
std::string Base64UrlEncode(ByteView bytes);

/// Accepts padded and unpadded input; rejects any character outside the alphabet.
bool Base64UrlDecode(const std::string& text, Bytes* out);

/// Standard base64 with padding, needed only for the WebSocket handshake keys
/// (RFC 6455 §4.1).
std::string Base64Encode(ByteView bytes);
bool Base64Decode(const std::string& text, Bytes* out);

}  // namespace tmirror
