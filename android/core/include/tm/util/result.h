#pragma once

#include <string>
#include <utility>

namespace tmirror {

/// Failure classes that survive all the way to the user-visible error states in
/// spec §15. Anything the UI must distinguish gets its own value here.
enum class ErrorKind {
  kNone = 0,
  kNetworkUnavailable,     // no connectivity / DNS / connect failure
  kTlsFailure,             // certificate, hostname or handshake failure
  kAuthFailed,             // bad signature, unregistered key, revoked device
  kAuthExpired,            // token expired; re-authentication required
  kPermissionDenied,       // 404/403 on an owned-resource check
  kNotFound,               // terminal or device gone
  kTerminalClosed,         // remote terminal ended
  kProtocolIncompatible,   // subprotocol/version negotiation failure
  kProtocolError,          // malformed or unexpected message
  kSyncFailure,            // offset_ahead / gap requiring a fresh subscription
  kInputRefused,           // terminal input refused (permanent for this session)
  kInputUndeliverable,     // terminal input refused (transient)
  kRateLimited,
  kServerError,
  kStorageError,           // local persistence failure
  kCancelled,
  kTimeout,
  kInvalidArgument,
  kInternal,
};

const char* ErrorKindName(ErrorKind kind);

/// True when retrying the same operation later can plausibly succeed.
bool ErrorKindIsRecoverable(ErrorKind kind);

class Status {
 public:
  Status() = default;
  Status(ErrorKind kind, std::string message)
      : kind_(kind), message_(std::move(message)) {}

  static Status Ok() { return Status(); }
  static Status Error(ErrorKind kind, std::string message) {
    return Status(kind, std::move(message));
  }

  bool ok() const { return kind_ == ErrorKind::kNone; }
  ErrorKind kind() const { return kind_; }
  const std::string& message() const { return message_; }

  /// Machine-readable server code (`offset_ahead`, `input_disabled`, ...) when the
  /// failure came from a relay error message. Empty otherwise.
  const std::string& code() const { return code_; }
  Status& set_code(std::string code) {
    code_ = std::move(code);
    return *this;
  }

  std::string ToString() const;

 private:
  ErrorKind kind_ = ErrorKind::kNone;
  std::string message_;
  std::string code_;
};

/// Minimal expected-like carrier. Deliberately not std::variant so the value type
/// need not be default-constructible-free of surprises on older toolchains.
template <typename T>
class Result {
 public:
  Result(T value) : value_(std::move(value)) {}          // NOLINT(google-explicit-constructor)
  Result(Status status) : status_(std::move(status)) {}  // NOLINT(google-explicit-constructor)

  bool ok() const { return status_.ok(); }
  const Status& status() const { return status_; }
  ErrorKind kind() const { return status_.kind(); }

  T& value() { return value_; }
  const T& value() const { return value_; }
  T&& take() { return std::move(value_); }

 private:
  T value_{};
  Status status_;
};

}  // namespace tmirror
