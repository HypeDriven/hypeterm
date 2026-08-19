#pragma once

#include <cstdint>
#include <string>
#include <vector>

#include "tm/term/utf8.h"
#include "tm/util/bytes.h"

namespace tmirror {
namespace term {

/// CSI/DCS parameters, including colon-separated sub-parameters (SGR 38:2:r:g:b).
///
/// Fixed capacity on purpose: parameter lists are untrusted input and a terminal that
/// allocates per parameter can be driven to exhaustion by `CSI 1;1;1;...m`.
class Params {
 public:
  static constexpr int kMaxParams = 32;
  static constexpr int kMaxSubParams = 8;
  static constexpr std::int32_t kMissing = -1;
  static constexpr std::int32_t kMaxValue = 65535;

  void Clear();
  /// Start a new parameter (on ';').
  void NextParam();
  /// Start a new sub-parameter of the current parameter (on ':').
  void NextSubParam();
  /// Accumulate a digit into the current (sub-)parameter.
  void PushDigit(std::uint8_t digit);
  /// True when the sequence had more parameters than the fixed capacity allows.
  bool overflowed() const { return overflowed_; }

  int count() const { return count_; }
  bool empty() const { return count_ == 0; }
  std::int32_t Get(int index, std::int32_t fallback) const;
  int SubCount(int index) const;
  std::int32_t GetSub(int index, int sub, std::int32_t fallback) const;
  /// Raw value including kMissing, for sequences that distinguish "omitted".
  std::int32_t Raw(int index) const;

  /// Private-parameter marker: '?', '<', '=', '>' or 0.
  std::uint8_t prefix() const { return prefix_; }
  void set_prefix(std::uint8_t prefix) { prefix_ = prefix; }
  /// Intermediate bytes (0x20-0x2F), e.g. ' ' in `CSI SP q`.
  const std::string& intermediates() const { return intermediates_; }
  void PushIntermediate(std::uint8_t byte);

 private:
  std::int32_t values_[kMaxParams][kMaxSubParams] = {};
  std::uint8_t sub_counts_[kMaxParams] = {};
  int count_ = 0;
  int current_sub_ = 0;
  bool overflowed_ = false;
  std::uint8_t prefix_ = 0;
  std::string intermediates_;
};

/// Callbacks from the state machine. Every one of them must tolerate arbitrary
/// values: unknown or malformed sequences are ignored, never fatal (spec §8.1).
class ParserHandler {
 public:
  virtual ~ParserHandler() = default;
  virtual void OnPrint(char32_t code_point) = 0;
  virtual void OnExecute(std::uint8_t control) = 0;
  virtual void OnEscape(const std::string& intermediates, std::uint8_t final_byte) = 0;
  virtual void OnCsi(const Params& params, std::uint8_t final_byte) = 0;
  /// OSC string, already split on ';'. `truncated` when the payload hit the bound.
  virtual void OnOsc(const std::vector<std::string>& parts, bool truncated) = 0;
  virtual void OnDcs(const Params& params, std::uint8_t final_byte,
                     const std::string& data, bool truncated) = 0;
};

/// ECMA-48 / DEC state machine, byte oriented and resumable at any boundary.
///
/// Chunk boundaries are invisible to it: state lives in the object, so a sequence may
/// be split anywhere, including inside a UTF-8 code point (spec §8.1, §16.1).
class Parser {
 public:
  struct Limits {
    /// Maximum OSC/DCS payload retained. Longer strings are still consumed to their
    /// terminator so the parser cannot be wedged, but the excess is discarded.
    std::size_t max_string_bytes = 8192;
  };

  explicit Parser(ParserHandler* handler) : handler_(handler) {}

  void SetLimits(const Limits& limits) { limits_ = limits; }
  void Feed(ByteView bytes);
  void Reset();

  /// Exposed for tests and diagnostics.
  enum class State {
    kGround,
    kEscape,
    kEscapeIntermediate,
    kCsiEntry,
    kCsiParam,
    kCsiIntermediate,
    kCsiIgnore,
    kOscString,
    kDcsEntry,
    kDcsParam,
    kDcsIntermediate,
    kDcsPassthrough,
    kDcsIgnore,
    kSosPmApcString,
  };
  State state() const { return state_; }

 private:
  void FeedByte(std::uint8_t byte);
  void Ground(std::uint8_t byte);
  void DispatchOsc();
  void DispatchDcs();
  void EnterState(State state);
  bool HandleControl(std::uint8_t byte);
  void FlushUtf8();

  ParserHandler* handler_;
  Limits limits_;
  State state_ = State::kGround;
  Params params_;
  std::string intermediates_;
  std::string string_buffer_;
  bool string_truncated_ = false;
  std::uint8_t dcs_final_ = 0;
  Utf8Decoder decoder_;
};

}  // namespace term
}  // namespace tmirror
