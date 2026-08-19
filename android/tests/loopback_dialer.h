#pragma once

#include <atomic>
#include <memory>
#include <mutex>
#include <string>

#include "tm/net/dialer.h"
#include "tm/net/socket.h"

namespace tmtest {

/// A dialer that reaches everything over loopback and hands the descriptor over.
///
/// It stands in for a tunnel in tests: the seam an embedded Tailscale node plugs into
/// is "give me a connected descriptor for this host and port", and everything above
/// that seam — descriptor adoption, TLS with a hostname that differs from the connect
/// address, HTTP, WebSocket, the whole controller — can be exercised without a tailnet.
///
/// Like a real tunnel it resolves the name itself, so tests can use a host name that
/// the device cannot resolve and thereby prove the dialer is the only path out.
class LoopbackDialer : public tmirror::net::Dialer {
 public:
  tmirror::Result<int> DialFd(const std::string& host, std::uint16_t port,
                              tmirror::Millis timeout_ms) override {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      last_host_ = host;
      last_port_ = port;
    }
    ++dials;
    if (!ready_) {
      return tmirror::Status::Error(tmirror::ErrorKind::kNetworkUnavailable,
                                    "test dialer is not ready");
    }
    auto socket = std::make_unique<tmirror::net::TcpTransport>(
        std::make_shared<tmirror::net::CancelSignal>());
    tmirror::Status connected = socket->Connect("127.0.0.1", port, timeout_ms);
    if (!connected.ok()) return connected;
    return socket->ReleaseFd();
  }

  bool ready() const override { return ready_; }
  std::string name() const override { return "the test tunnel"; }
  void set_ready(bool ready) { ready_ = ready; }

  std::string last_host() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return last_host_;
  }
  std::uint16_t last_port() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return last_port_;
  }

  std::atomic<int> dials{0};

 private:
  mutable std::mutex mutex_;
  std::string last_host_;
  std::uint16_t last_port_ = 0;
  std::atomic<bool> ready_{true};
};

}  // namespace tmtest
