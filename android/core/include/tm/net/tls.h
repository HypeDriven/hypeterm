#pragma once

#include <memory>
#include <string>
#include <vector>

#include "tm/net/dialer.h"
#include "tm/net/socket.h"

namespace tmirror {
namespace net {

struct TlsConfig {
  /// Name verified against the certificate and sent as SNI. Never optional.
  std::string hostname;
  /// PEM trust anchors added to the verification store.
  ///
  /// On Android the platform trust store is not on disk in a form OpenSSL can read,
  /// so the host layer exports the anchors from `AndroidCAStore` and supplies them
  /// here. Tests use the same mechanism to trust a local fake relay. This *adds*
  /// anchors; it never disables verification, which spec §7.4 forbids outright.
  std::vector<std::string> trust_anchors_pem;
  /// Also load OpenSSL's built-in default paths (true on desktop/CI, harmless
  /// elsewhere).
  bool use_default_trust_store = true;
  std::string alpn_protocol = "http/1.1";
  Millis handshake_timeout_ms = 15000;
};

/// TLS client transport. There is deliberately no "insecure" or "skip verification"
/// option anywhere in this type (spec §7.4).
class TlsTransport : public Transport {
 public:
  ~TlsTransport() override;

  static Result<std::unique_ptr<TlsTransport>> Establish(std::unique_ptr<TcpTransport> tcp,
                                                         const TlsConfig& config);

  Result<std::size_t> Read(std::uint8_t* buffer, std::size_t size, Millis timeout_ms) override;
  Status WriteAll(ByteView data, Millis timeout_ms) override;
  void Close() override;
  void Cancel() override;
  bool is_open() const override;
  void SetInterrupt(Notifier* notifier) override;

  /// Negotiated protocol version name, for diagnostics.
  std::string protocol_version() const;

 private:
  TlsTransport() = default;
  struct Impl;
  std::unique_ptr<Impl> impl_;
};

/// How a connection is made, beyond where it goes.
struct TransportOptions {
  Millis connect_timeout_ms = 15000;

  /// When set, the connection is dialled through this tunnel instead of the network
  /// stack. Not owned; it must outlive every transport it opens.
  Dialer* dialer = nullptr;

  /// Permit `http://` or `ws://` *through a tunnel*.
  ///
  /// Off by default. A tunnel like Tailscale authenticates its peers and encrypts the
  /// path, so cleartext inside it is not the same risk as cleartext on the internet —
  /// but TLS inside the tunnel is still defence in depth, and `tailscale cert` makes
  /// it easy, so this stays an explicit choice (spec §7.4). It never affects a direct
  /// connection.
  bool allow_cleartext_over_tunnel = false;
};

/// Open a transport for a URL: TLS for https/wss, plain TCP otherwise.
///
/// Without a tunnel, plain TCP is reachable only for `http://`/`ws://` to a loopback
/// host — production endpoints are always TLS (spec §7.4). With a tunnel, the loopback
/// exception does *not* apply: a tunnel's descriptor is not a loopback address, and
/// cleartext through it requires `allow_cleartext_over_tunnel`.
Result<std::unique_ptr<Transport>> OpenTransport(const std::string& scheme,
                                                 const std::string& host, std::uint16_t port,
                                                 const TlsConfig& tls_config,
                                                 std::shared_ptr<CancelSignal> cancel,
                                                 const TransportOptions& options);

/// True for hosts that may legitimately be reached without TLS during development.
bool IsLoopbackHost(const std::string& host);

}  // namespace net
}  // namespace tmirror
