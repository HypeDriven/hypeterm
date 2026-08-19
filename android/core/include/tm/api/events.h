#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "tm/util/bytes.h"
#include "tm/util/result.h"
#include "tm/util/time.h"

namespace tmirror {
namespace api {

/// Subprotocol names (relay spec §6). Version 2 is what this client uses: it is the
/// only one that carries terminal input.
constexpr const char kMirrorSubprotocolV2[] = "terminal-relay.mirror.v2";
constexpr const char kMirrorSubprotocolV1[] = "terminal-relay.mirror.v1";

/// The relay's replay window (relay spec §7.1): decimal 1,500,000 bytes, not 1.5 MiB.
/// Local scrollback outlives it, so a `gap` means the screen must be rebuilt.
constexpr std::uint64_t kReplayWindowBytes = 1500000;

struct TerminalInfo {
  std::string terminal_id;
  std::string device_id;
  std::string identity_id;
  std::string label;
  std::string local_ref;
  std::string state;  // "open" or "closed"
  std::string term;
  std::string close_reason;
  std::string created_at;
  std::string last_activity_at;
  std::uint32_t columns = 0;
  std::uint32_t rows = 0;
  std::uint64_t earliest_offset = 0;
  std::uint64_t next_offset = 0;
  std::uint64_t durable_offset = 0;
  std::uint64_t retained_bytes = 0;
  bool accepts_input = false;

  bool open() const { return state == "open"; }
};

struct TerminalPage {
  std::vector<TerminalInfo> terminals;
  std::string next_cursor;
};

struct DeviceInfo {
  std::string device_id;
  std::string identity_id;
  std::string name;
  std::string role;
  std::string key_fingerprint;
  std::string created_at;
  std::string revoked_at;
};

struct AccessToken {
  std::string token;
  std::vector<std::string> scopes;
  std::string principal;
  std::string principal_id;
  /// Absolute wall-clock expiry, derived from `expires_in` at receipt.
  Millis expires_at_unix_ms = 0;

  bool HasScope(const std::string& scope) const;
  bool valid() const { return !token.empty(); }
  /// True when the token should be replaced before use, with a safety margin.
  bool NeedsRefresh(Millis now_unix_ms, Millis margin_ms = 60000) const {
    return !valid() || now_unix_ms + margin_ms >= expires_at_unix_ms;
  }
};

/// The relay's response to `POST /v1/auth/challenges`.
struct Challenge {
  std::string challenge_id;
  Bytes challenge;
  std::string signature_context;
  /// Exact bytes to sign. Verified field by field before the key is used.
  Bytes signing_input;
  std::string expires_at;
  std::string key_fingerprint;
};

/// What `subscribed` tells the client (relay spec §6.2).
struct SubscribedInfo {
  std::string terminal_id;
  std::uint64_t requested_from_offset = 0;
  std::uint64_t replay_start_offset = 0;
  std::uint64_t next_offset = 0;
  std::uint64_t durable_offset = 0;
  std::uint64_t earliest_offset = 0;
  std::string terminal_state;
  std::string label;
  std::string term;
  std::uint32_t columns = 0;
  std::uint32_t rows = 0;
  /// The publisher's declared opt-in. Not sufficient on its own to send input.
  bool accepts_input = false;
  /// Whether *this* subscription may send input right now. A client that ignores
  /// this and types anyway gets its frames refused (relay spec §4.5).
  bool input_available = false;
};

/// Protocol limits the relay advertises in `ready` (relay spec §6.1).
struct RelayLimits {
  std::uint64_t max_output_frame_bytes = 262144;
  std::uint64_t max_control_message_bytes = 65536;
  std::uint64_t max_input_frame_bytes = 4096;
  std::uint64_t replay_capacity_bytes = kReplayWindowBytes;
  std::uint64_t heartbeat_interval_seconds = 20;
  std::uint64_t heartbeat_timeout_seconds = 60;
  bool input_supported = false;
};

/// Normalized inbound events (spec §7.2), expressed in the relay's terms: an offset
/// is a byte count, not a message counter.
enum class MirrorEventKind {
  kReady,
  kSubscribed,
  kOutput,
  kGap,
  kDurable,
  kResize,
  kTerminalClosed,
  kInputAck,
  kNotice,
  kError,
};

struct MirrorEvent {
  MirrorEventKind kind = MirrorEventKind::kNotice;
  // kOutput
  std::uint64_t start_offset = 0;
  ByteView payload;
  // kGap
  std::uint64_t requested_from_offset = 0;
  std::uint64_t available_from_offset = 0;
  // kDurable
  std::uint64_t durable_offset = 0;
  // kResize
  std::uint32_t columns = 0;
  std::uint32_t rows = 0;
  // kInputAck
  std::uint64_t accepted_through = 0;
  std::uint64_t relay_sequence = 0;
  // kSubscribed / kReady
  SubscribedInfo subscribed;
  RelayLimits limits;
  // kTerminalClosed / kError / kNotice
  std::string code;
  std::string message;
  Status status;
};

/// Maps a relay error code to the user-visible error classes in spec §15.
ErrorKind ErrorKindForRelayCode(const std::string& code);
/// True for input refusals the client may retry later (relay spec §6.3).
bool IsTransientInputRefusal(const std::string& code);

}  // namespace api
}  // namespace tmirror
