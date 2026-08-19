#include "tm/term/parser.h"

#include <algorithm>

namespace tmirror {
namespace term {

// ---------------------------------------------------------------------- Params

void Params::Clear() {
  count_ = 0;
  current_sub_ = 0;
  overflowed_ = false;
  prefix_ = 0;
  intermediates_.clear();
  for (int i = 0; i < kMaxParams; ++i) {
    sub_counts_[i] = 0;
    for (int j = 0; j < kMaxSubParams; ++j) values_[i][j] = kMissing;
  }
}

void Params::NextParam() {
  if (count_ == 0) count_ = 1;  // a leading ';' means "first parameter omitted"
  if (count_ < kMaxParams) {
    ++count_;
    sub_counts_[count_ - 1] = 0;
    current_sub_ = 0;
  } else {
    overflowed_ = true;
  }
}

void Params::NextSubParam() {
  if (count_ == 0) count_ = 1;
  int index = count_ - 1;
  if (current_sub_ + 1 < kMaxSubParams) {
    ++current_sub_;
    if (sub_counts_[index] <= current_sub_) {
      sub_counts_[index] = static_cast<std::uint8_t>(current_sub_ + 1);
    }
  } else {
    overflowed_ = true;
  }
}

void Params::PushDigit(std::uint8_t digit) {
  if (count_ == 0) {
    count_ = 1;
    current_sub_ = 0;
    sub_counts_[0] = 1;
  }
  int index = count_ - 1;
  if (index >= kMaxParams) {
    overflowed_ = true;
    return;
  }
  if (sub_counts_[index] <= current_sub_) {
    sub_counts_[index] = static_cast<std::uint8_t>(current_sub_ + 1);
  }
  std::int32_t& slot = values_[index][current_sub_];
  if (slot == kMissing) slot = 0;
  if (slot <= kMaxValue) {
    slot = slot * 10 + digit;
    // Saturate rather than overflow: an absurd parameter is clamped, and the caller
    // clamps again against the grid.
    if (slot > kMaxValue) slot = kMaxValue;
  }
}

std::int32_t Params::Get(int index, std::int32_t fallback) const {
  if (index < 0 || index >= count_) return fallback;
  std::int32_t value = values_[index][0];
  return value == kMissing ? fallback : value;
}

std::int32_t Params::Raw(int index) const {
  if (index < 0 || index >= count_) return kMissing;
  return values_[index][0];
}

int Params::SubCount(int index) const {
  if (index < 0 || index >= count_) return 0;
  return sub_counts_[index];
}

std::int32_t Params::GetSub(int index, int sub, std::int32_t fallback) const {
  if (index < 0 || index >= count_) return fallback;
  if (sub < 0 || sub >= sub_counts_[index]) return fallback;
  std::int32_t value = values_[index][sub];
  return value == kMissing ? fallback : value;
}

void Params::PushIntermediate(std::uint8_t byte) {
  if (intermediates_.size() < 4) {
    intermediates_.push_back(static_cast<char>(byte));
  } else {
    overflowed_ = true;
  }
}

// ---------------------------------------------------------------------- Parser

void Parser::Reset() {
  state_ = State::kGround;
  params_.Clear();
  intermediates_.clear();
  string_buffer_.clear();
  string_truncated_ = false;
  dcs_final_ = 0;
  decoder_.Reset();
}

void Parser::EnterState(State state) {
  state_ = state;
  switch (state) {
    case State::kEscape:
      params_.Clear();
      intermediates_.clear();
      break;
    case State::kCsiEntry:
    case State::kDcsEntry:
      params_.Clear();
      intermediates_.clear();
      break;
    case State::kOscString:
    case State::kSosPmApcString:
      string_buffer_.clear();
      string_truncated_ = false;
      break;
    case State::kDcsPassthrough:
      string_buffer_.clear();
      string_truncated_ = false;
      break;
    default:
      break;
  }
}

void Parser::FlushUtf8() {
  char32_t code_point = 0;
  if (decoder_.Flush(&code_point)) handler_->OnPrint(code_point);
}

void Parser::Feed(ByteView bytes) {
  for (std::size_t i = 0; i < bytes.size(); ++i) FeedByte(bytes[i]);
}

/// Controls that act in (almost) every state. Returns true when handled.
bool Parser::HandleControl(std::uint8_t byte) {
  if (byte == 0x1B) {  // ESC always restarts a sequence
    FlushUtf8();
    if (state_ == State::kDcsPassthrough) DispatchDcs();
    EnterState(State::kEscape);
    return true;
  }
  if (byte == 0x18 || byte == 0x1A) {  // CAN, SUB abort the current sequence
    FlushUtf8();
    if (state_ == State::kDcsPassthrough) DispatchDcs();
    EnterState(State::kGround);
    return true;
  }
  return false;
}

void Parser::FeedByte(std::uint8_t byte) {
  switch (state_) {
    case State::kGround:
      Ground(byte);
      return;

    case State::kEscape:
      if (HandleControl(byte)) return;
      if (byte < 0x20) {
        handler_->OnExecute(byte);
        return;
      }
      if (byte >= 0x20 && byte <= 0x2F) {
        intermediates_.push_back(static_cast<char>(byte));
        if (intermediates_.size() > 4) {
          // Absurd intermediates: consume the rest of the sequence and ignore it.
          EnterState(State::kCsiIgnore);
          return;
        }
        state_ = State::kEscapeIntermediate;
        return;
      }
      if (byte == '[') {
        EnterState(State::kCsiEntry);
        return;
      }
      if (byte == ']') {
        EnterState(State::kOscString);
        return;
      }
      if (byte == 'P') {
        EnterState(State::kDcsEntry);
        return;
      }
      if (byte == 'X' || byte == '^' || byte == '_') {  // SOS, PM, APC
        EnterState(State::kSosPmApcString);
        return;
      }
      if (byte >= 0x30 && byte <= 0x7E) {
        handler_->OnEscape(intermediates_, byte);
        EnterState(State::kGround);
        return;
      }
      // 0x7F (DEL) and anything else is ignored while remaining in this state.
      return;

    case State::kEscapeIntermediate:
      if (HandleControl(byte)) return;
      if (byte < 0x20) {
        handler_->OnExecute(byte);
        return;
      }
      if (byte >= 0x20 && byte <= 0x2F) {
        if (intermediates_.size() < 4) intermediates_.push_back(static_cast<char>(byte));
        return;
      }
      if (byte >= 0x30 && byte <= 0x7E) {
        handler_->OnEscape(intermediates_, byte);
        EnterState(State::kGround);
        return;
      }
      return;

    case State::kCsiEntry:
    case State::kCsiParam:
      if (HandleControl(byte)) return;
      if (byte < 0x20) {
        handler_->OnExecute(byte);
        return;
      }
      if (byte >= 0x30 && byte <= 0x39) {
        params_.PushDigit(static_cast<std::uint8_t>(byte - '0'));
        state_ = State::kCsiParam;
        return;
      }
      if (byte == ';') {
        params_.NextParam();
        state_ = State::kCsiParam;
        return;
      }
      if (byte == ':') {
        params_.NextSubParam();
        state_ = State::kCsiParam;
        return;
      }
      if (byte >= 0x3C && byte <= 0x3F) {  // '<', '=', '>', '?'
        if (state_ == State::kCsiEntry && params_.prefix() == 0) {
          params_.set_prefix(byte);
          state_ = State::kCsiParam;
          return;
        }
        // A private marker anywhere else makes the sequence unparseable.
        state_ = State::kCsiIgnore;
        return;
      }
      if (byte >= 0x20 && byte <= 0x2F) {
        params_.PushIntermediate(byte);
        state_ = State::kCsiIntermediate;
        return;
      }
      if (byte >= 0x40 && byte <= 0x7E) {
        handler_->OnCsi(params_, byte);
        EnterState(State::kGround);
        return;
      }
      state_ = State::kCsiIgnore;
      return;

    case State::kCsiIntermediate:
      if (HandleControl(byte)) return;
      if (byte < 0x20) {
        handler_->OnExecute(byte);
        return;
      }
      if (byte >= 0x20 && byte <= 0x2F) {
        params_.PushIntermediate(byte);
        return;
      }
      if (byte >= 0x40 && byte <= 0x7E) {
        handler_->OnCsi(params_, byte);
        EnterState(State::kGround);
        return;
      }
      state_ = State::kCsiIgnore;
      return;

    case State::kCsiIgnore:
      if (HandleControl(byte)) return;
      if (byte < 0x20) {
        handler_->OnExecute(byte);
        return;
      }
      if (byte >= 0x40 && byte <= 0x7E) EnterState(State::kGround);
      return;

    case State::kOscString:
      if (byte == 0x07) {  // BEL terminates an OSC string
        DispatchOsc();
        EnterState(State::kGround);
        return;
      }
      if (byte == 0x1B) {
        // ESC \ (ST) is the other terminator; anything else after ESC restarts.
        DispatchOsc();
        EnterState(State::kEscape);
        return;
      }
      if (byte == 0x18 || byte == 0x1A) {
        EnterState(State::kGround);
        return;
      }
      if (byte < 0x20) return;  // other controls inside a string are ignored
      if (string_buffer_.size() < limits_.max_string_bytes) {
        string_buffer_.push_back(static_cast<char>(byte));
      } else {
        string_truncated_ = true;  // keep scanning for the terminator
      }
      return;

    case State::kSosPmApcString:
      // Consumed and discarded, but still scanned for its terminator so the parser
      // never wedges (spec §8.1).
      if (byte == 0x07) {
        EnterState(State::kGround);
        return;
      }
      if (byte == 0x1B) {
        EnterState(State::kEscape);
        return;
      }
      if (byte == 0x18 || byte == 0x1A) EnterState(State::kGround);
      return;

    case State::kDcsEntry:
    case State::kDcsParam:
      if (HandleControl(byte)) return;
      if (byte < 0x20) return;
      if (byte >= 0x30 && byte <= 0x39) {
        params_.PushDigit(static_cast<std::uint8_t>(byte - '0'));
        state_ = State::kDcsParam;
        return;
      }
      if (byte == ';') {
        params_.NextParam();
        state_ = State::kDcsParam;
        return;
      }
      if (byte == ':') {
        params_.NextSubParam();
        state_ = State::kDcsParam;
        return;
      }
      if (byte >= 0x3C && byte <= 0x3F) {
        if (state_ == State::kDcsEntry && params_.prefix() == 0) {
          params_.set_prefix(byte);
          state_ = State::kDcsParam;
          return;
        }
        state_ = State::kDcsIgnore;
        return;
      }
      if (byte >= 0x20 && byte <= 0x2F) {
        params_.PushIntermediate(byte);
        state_ = State::kDcsIntermediate;
        return;
      }
      if (byte >= 0x40 && byte <= 0x7E) {
        dcs_final_ = byte;
        EnterState(State::kDcsPassthrough);
        return;
      }
      state_ = State::kDcsIgnore;
      return;

    case State::kDcsIntermediate:
      if (HandleControl(byte)) return;
      if (byte >= 0x20 && byte <= 0x2F) {
        params_.PushIntermediate(byte);
        return;
      }
      if (byte >= 0x40 && byte <= 0x7E) {
        dcs_final_ = byte;
        EnterState(State::kDcsPassthrough);
        return;
      }
      if (byte >= 0x30 && byte <= 0x3F) {
        state_ = State::kDcsIgnore;
        return;
      }
      return;

    case State::kDcsPassthrough:
      if (byte == 0x1B) {
        DispatchDcs();
        EnterState(State::kEscape);
        return;
      }
      if (byte == 0x18 || byte == 0x1A) {
        DispatchDcs();
        EnterState(State::kGround);
        return;
      }
      if (byte == 0x07) {
        DispatchDcs();
        EnterState(State::kGround);
        return;
      }
      if (string_buffer_.size() < limits_.max_string_bytes) {
        string_buffer_.push_back(static_cast<char>(byte));
      } else {
        string_truncated_ = true;
      }
      return;

    case State::kDcsIgnore:
      if (byte == 0x1B) {
        EnterState(State::kEscape);
        return;
      }
      if (byte == 0x18 || byte == 0x1A) EnterState(State::kGround);
      return;
  }
}

void Parser::Ground(std::uint8_t byte) {
  if (byte == 0x1B) {
    FlushUtf8();
    EnterState(State::kEscape);
    return;
  }
  if (byte < 0x20) {
    FlushUtf8();
    handler_->OnExecute(byte);
    return;
  }
  if (byte == 0x7F) {
    // DEL is ignored in a UTF-8 xterm profile.
    return;
  }

  char32_t code_point = 0;
  bool reprocess = false;
  if (decoder_.Feed(byte, &code_point, &reprocess)) {
    handler_->OnPrint(code_point);
    if (reprocess) {
      // The byte that ended the ill-formed sequence gets a full second look: it may
      // begin a valid sequence, but it may equally be ESC or a C0 control, and
      // feeding it straight back to the decoder would print it as text.
      Ground(byte);
    }
  }
}

void Parser::DispatchOsc() {
  // Bounded: an OSC full of separators must not allocate one string per separator.
  // Once the cap is reached the remainder, separators included, stays in the last
  // part — which is what OSC 52 and OSC 4 payloads want anyway.
  constexpr std::size_t kMaxParts = 16;
  std::vector<std::string> parts;
  std::string current;
  for (char c : string_buffer_) {
    if (c == ';' && parts.size() + 1 < kMaxParts) {
      parts.push_back(std::move(current));
      current.clear();
    } else {
      current.push_back(c);
    }
  }
  parts.push_back(std::move(current));
  handler_->OnOsc(parts, string_truncated_);
  string_buffer_.clear();
  string_truncated_ = false;
}

void Parser::DispatchDcs() {
  handler_->OnDcs(params_, dcs_final_, string_buffer_, string_truncated_);
  string_buffer_.clear();
  string_truncated_ = false;
  dcs_final_ = 0;
}

}  // namespace term
}  // namespace tmirror
