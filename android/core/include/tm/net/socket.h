#pragma once

#include <atomic>
#include <cstdint>
#include <memory>
#include <string>

#include "tm/util/bytes.h"
#include "tm/util/result.h"
#include "tm/util/time.h"

namespace tmirror {
namespace net {

class Notifier;

/// Byte transport: a plain TCP socket or a TLS session over one.
///
/// Every operation takes a deadline and every blocking wait is also woken by
/// `Cancel()`, because a connection that outlives its generation must be able to die
/// immediately rather than at the next timeout (spec §11).
class Transport {
 public:
  virtual ~Transport() = default;
  /// Reads up to `size` bytes. Returns 0 only at end of stream.
  virtual Result<std::size_t> Read(std::uint8_t* buffer, std::size_t size, Millis timeout_ms) = 0;
  virtual Status WriteAll(ByteView data, Millis timeout_ms) = 0;
  virtual void Close() = 0;
  /// Wakes any blocked Read/Write on any thread and fails subsequent operations.
  virtual void Cancel() = 0;
  virtual bool is_open() const = 0;
  /// When set, a notification makes a pending Read return `kTimeout` promptly so the
  /// caller can service its outbound queue without polling (spec §6.2).
  virtual void SetInterrupt(Notifier* notifier) = 0;
};

/// Cancellation token shared between a transport and its owner.
class CancelSignal {
 public:
  CancelSignal();
  ~CancelSignal();
  CancelSignal(const CancelSignal&) = delete;
  CancelSignal& operator=(const CancelSignal&) = delete;

  void Cancel();
  bool cancelled() const { return cancelled_.load(); }
  /// File descriptor that becomes readable when cancelled; poll it alongside a socket.
  int fd() const { return read_fd_; }

 private:
  int read_fd_ = -1;
  int write_fd_ = -1;
  std::atomic<bool> cancelled_{false};
};

/// Resettable wake-up channel used to interrupt a blocking read when the outbound
/// queue gains work. Distinct from CancelSignal, which is terminal and one-shot.
class Notifier {
 public:
  Notifier();
  ~Notifier();
  Notifier(const Notifier&) = delete;
  Notifier& operator=(const Notifier&) = delete;

  void Notify();
  void Drain();
  int fd() const { return read_fd_; }

 private:
  int read_fd_ = -1;
  int write_fd_ = -1;
  std::atomic<bool> pending_{false};
};

class TcpTransport : public Transport {
 public:
  explicit TcpTransport(std::shared_ptr<CancelSignal> cancel);
  ~TcpTransport() override;

  Status Connect(const std::string& host, std::uint16_t port, Millis timeout_ms);

  /// Takes ownership of an already-connected stream socket. A tunnel dials in user
  /// space and hands the result over this way, so nothing above here needs to know
  /// whether the peer was reached through the network stack or through a tunnel.
  Status Adopt(int fd);

  /// Gives up ownership of the descriptor without closing it. Used by dialers built
  /// on top of an ordinary connect.
  int ReleaseFd();

  Result<std::size_t> Read(std::uint8_t* buffer, std::size_t size, Millis timeout_ms) override;
  Status WriteAll(ByteView data, Millis timeout_ms) override;
  void Close() override;
  void Cancel() override;
  bool is_open() const override { return fd_ >= 0; }
  void SetInterrupt(Notifier* notifier) override { interrupt_ = notifier; }

  int fd() const { return fd_; }
  const std::shared_ptr<CancelSignal>& cancel_signal() const { return cancel_; }

  /// Wait until the socket is readable/writable or the deadline passes.
  /// Exposed for the TLS layer, which drives the same descriptor.
  Status Wait(bool for_read, Millis timeout_ms);

 private:
  int fd_ = -1;
  std::shared_ptr<CancelSignal> cancel_;
  Notifier* interrupt_ = nullptr;
};

}  // namespace net
}  // namespace tmirror
