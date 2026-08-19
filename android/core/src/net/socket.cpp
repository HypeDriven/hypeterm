#include "tm/net/socket.h"

#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#include "tm/util/log.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace net {
namespace {

constexpr const char kTag[] = "tm.net";

Status SystemError(ErrorKind kind, const char* what, int error_number) {
  // strerror text is safe to log: it never contains payload or credentials.
  return Status::Error(kind, std::string(what) + ": " + std::strerror(error_number));
}

bool SetNonBlocking(int fd) {
  int flags = ::fcntl(fd, F_GETFL, 0);
  if (flags < 0) return false;
  return ::fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

Millis MonotonicNow() { return Clock::System()->MonotonicMillis(); }

}  // namespace

CancelSignal::CancelSignal() {
  int fds[2];
  if (::pipe(fds) == 0) {
    read_fd_ = fds[0];
    write_fd_ = fds[1];
    SetNonBlocking(read_fd_);
    SetNonBlocking(write_fd_);
  }
}

CancelSignal::~CancelSignal() {
  if (read_fd_ >= 0) ::close(read_fd_);
  if (write_fd_ >= 0) ::close(write_fd_);
}

void CancelSignal::Cancel() {
  bool expected = false;
  if (!cancelled_.compare_exchange_strong(expected, true)) return;
  if (write_fd_ >= 0) {
    const char byte = 1;
    ssize_t ignored = ::write(write_fd_, &byte, 1);
    (void)ignored;
  }
}

Notifier::Notifier() {
  int fds[2];
  if (::pipe(fds) == 0) {
    read_fd_ = fds[0];
    write_fd_ = fds[1];
    SetNonBlocking(read_fd_);
    SetNonBlocking(write_fd_);
  }
}

Notifier::~Notifier() {
  if (read_fd_ >= 0) ::close(read_fd_);
  if (write_fd_ >= 0) ::close(write_fd_);
}

void Notifier::Notify() {
  bool expected = false;
  if (!pending_.compare_exchange_strong(expected, true)) return;
  if (write_fd_ >= 0) {
    const char byte = 1;
    ssize_t ignored = ::write(write_fd_, &byte, 1);
    (void)ignored;
  }
}

void Notifier::Drain() {
  pending_.store(false);
  char buffer[64];
  while (read_fd_ >= 0 && ::read(read_fd_, buffer, sizeof(buffer)) > 0) {
  }
}

TcpTransport::TcpTransport(std::shared_ptr<CancelSignal> cancel) : cancel_(std::move(cancel)) {
  if (!cancel_) cancel_ = std::make_shared<CancelSignal>();
}

TcpTransport::~TcpTransport() { Close(); }

Status TcpTransport::Connect(const std::string& host, std::uint16_t port, Millis timeout_ms) {
  if (cancel_->cancelled()) return Status::Error(ErrorKind::kCancelled, "connect cancelled");

  struct addrinfo hints;
  std::memset(&hints, 0, sizeof(hints));
  hints.ai_family = AF_UNSPEC;
  hints.ai_socktype = SOCK_STREAM;
  hints.ai_protocol = IPPROTO_TCP;

  struct addrinfo* results = nullptr;
  std::string service = Uint64ToString(port);
  int rc = ::getaddrinfo(host.c_str(), service.c_str(), &hints, &results);
  if (rc != 0 || results == nullptr) {
    return Status::Error(ErrorKind::kNetworkUnavailable,
                         std::string("cannot resolve host: ") + ::gai_strerror(rc));
  }

  Status last = Status::Error(ErrorKind::kNetworkUnavailable, "no addresses for host");
  Millis deadline = timeout_ms < 0 ? -1 : MonotonicNow() + timeout_ms;

  for (struct addrinfo* candidate = results; candidate != nullptr;
       candidate = candidate->ai_next) {
    if (cancel_->cancelled()) {
      last = Status::Error(ErrorKind::kCancelled, "connect cancelled");
      break;
    }
    int fd = ::socket(candidate->ai_family, candidate->ai_socktype, candidate->ai_protocol);
    if (fd < 0) {
      last = SystemError(ErrorKind::kNetworkUnavailable, "socket", errno);
      continue;
    }
    if (!SetNonBlocking(fd)) {
      ::close(fd);
      last = SystemError(ErrorKind::kInternal, "fcntl", errno);
      continue;
    }
    int one = 1;
    ::setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

    int result = ::connect(fd, candidate->ai_addr, candidate->ai_addrlen);
    if (result != 0 && errno == EINPROGRESS) {
      fd_ = fd;
      Millis remaining = deadline < 0 ? -1 : deadline - MonotonicNow();
      Status wait = Wait(false, remaining);
      if (!wait.ok()) {
        fd_ = -1;
        ::close(fd);
        last = wait;
        continue;
      }
      int error = 0;
      socklen_t length = sizeof(error);
      if (::getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) != 0 || error != 0) {
        fd_ = -1;
        ::close(fd);
        last = SystemError(ErrorKind::kNetworkUnavailable, "connect", error != 0 ? error : errno);
        continue;
      }
      result = 0;
    } else if (result != 0) {
      ::close(fd);
      last = SystemError(ErrorKind::kNetworkUnavailable, "connect", errno);
      continue;
    }

    fd_ = fd;
    ::freeaddrinfo(results);
    TM_LOG_DEBUG(kTag, "connected to %s:%u", host.c_str(), static_cast<unsigned>(port));
    return Status::Ok();
  }

  ::freeaddrinfo(results);
  return last;
}

Status TcpTransport::Adopt(int fd) {
  if (fd < 0) return Status::Error(ErrorKind::kInvalidArgument, "not a descriptor");
  Close();
  if (!SetNonBlocking(fd)) {
    ::close(fd);
    return SystemError(ErrorKind::kInternal, "fcntl", errno);
  }
  fd_ = fd;
  return Status::Ok();
}

int TcpTransport::ReleaseFd() {
  int fd = fd_;
  fd_ = -1;
  return fd;
}

Status TcpTransport::Wait(bool for_read, Millis timeout_ms) {
  if (fd_ < 0) return Status::Error(ErrorKind::kNetworkUnavailable, "socket is closed");
  struct pollfd fds[3];
  fds[0].fd = fd_;
  fds[0].events = static_cast<short>(for_read ? POLLIN : POLLOUT);
  fds[0].revents = 0;
  int count = 1;
  int cancel_index = -1;
  int interrupt_index = -1;
  if (cancel_->fd() >= 0) {
    cancel_index = count;
    fds[count].fd = cancel_->fd();
    fds[count].events = POLLIN;
    fds[count].revents = 0;
    ++count;
  }
  if (interrupt_ != nullptr && interrupt_->fd() >= 0) {
    interrupt_index = count;
    fds[count].fd = interrupt_->fd();
    fds[count].events = POLLIN;
    fds[count].revents = 0;
    ++count;
  }

  Millis deadline = timeout_ms < 0 ? -1 : MonotonicNow() + timeout_ms;
  while (true) {
    if (cancel_->cancelled()) {
      return Status::Error(ErrorKind::kCancelled, "operation cancelled");
    }
    int wait_ms = -1;
    if (deadline >= 0) {
      Millis remaining = deadline - MonotonicNow();
      if (remaining <= 0) return Status::Error(ErrorKind::kTimeout, "socket wait timed out");
      wait_ms = static_cast<int>(remaining);
    }
    int rc = ::poll(fds, static_cast<nfds_t>(count), wait_ms);
    if (rc < 0) {
      if (errno == EINTR) continue;
      return SystemError(ErrorKind::kNetworkUnavailable, "poll", errno);
    }
    if (rc == 0) return Status::Error(ErrorKind::kTimeout, "socket wait timed out");
    if (cancel_index >= 0 && (fds[cancel_index].revents & POLLIN) != 0) {
      return Status::Error(ErrorKind::kCancelled, "operation cancelled");
    }
    if (interrupt_index >= 0 && (fds[interrupt_index].revents & POLLIN) != 0) {
      interrupt_->Drain();
      // Not an error: the caller has outbound work to do and will come back.
      return Status::Error(ErrorKind::kTimeout, "interrupted to service the outbound queue");
    }
    if (fds[0].revents != 0) return Status::Ok();
  }
}

Result<std::size_t> TcpTransport::Read(std::uint8_t* buffer, std::size_t size,
                                       Millis timeout_ms) {
  if (fd_ < 0) return Status::Error(ErrorKind::kNetworkUnavailable, "socket is closed");
  while (true) {
    ssize_t n = ::recv(fd_, buffer, size, 0);
    if (n > 0) return static_cast<std::size_t>(n);
    if (n == 0) return static_cast<std::size_t>(0);  // orderly shutdown
    if (errno == EINTR) continue;
    if (errno == EAGAIN || errno == EWOULDBLOCK) {
      Status wait = Wait(true, timeout_ms);
      if (!wait.ok()) return wait;
      continue;
    }
    return SystemError(ErrorKind::kNetworkUnavailable, "recv", errno);
  }
}

Status TcpTransport::WriteAll(ByteView data, Millis timeout_ms) {
  if (fd_ < 0) return Status::Error(ErrorKind::kNetworkUnavailable, "socket is closed");
  std::size_t written = 0;
  Millis deadline = timeout_ms < 0 ? -1 : MonotonicNow() + timeout_ms;
  while (written < data.size()) {
    ssize_t n = ::send(fd_, data.data() + written, data.size() - written, MSG_NOSIGNAL);
    if (n > 0) {
      written += static_cast<std::size_t>(n);
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
      Millis remaining = deadline < 0 ? -1 : deadline - MonotonicNow();
      Status wait = Wait(false, remaining);
      if (!wait.ok()) return wait;
      continue;
    }
    return SystemError(ErrorKind::kNetworkUnavailable, "send", errno);
  }
  return Status::Ok();
}

void TcpTransport::Close() {
  if (fd_ >= 0) {
    ::close(fd_);
    fd_ = -1;
  }
}

void TcpTransport::Cancel() { cancel_->Cancel(); }

}  // namespace net
}  // namespace tmirror
