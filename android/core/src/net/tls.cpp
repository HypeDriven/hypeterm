#include "tm/net/tls.h"

#include <openssl/bio.h>
#include <openssl/err.h>
#include <openssl/ssl.h>
#include <openssl/x509v3.h>

#include <cstring>

#include "tm/util/log.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace net {
namespace {

constexpr const char kTag[] = "tm.tls";

std::string OpenSslError() {
  unsigned long code = ERR_get_error();
  if (code == 0) return "unknown TLS error";
  char buffer[256];
  ERR_error_string_n(code, buffer, sizeof(buffer));
  ERR_clear_error();
  return std::string(buffer);
}

}  // namespace

struct TlsTransport::Impl {
  std::unique_ptr<TcpTransport> tcp;
  SSL_CTX* context = nullptr;
  SSL* ssl = nullptr;

  ~Impl() {
    if (ssl != nullptr) {
      SSL_free(ssl);
      ssl = nullptr;
    }
    if (context != nullptr) {
      SSL_CTX_free(context);
      context = nullptr;
    }
  }
};

TlsTransport::~TlsTransport() = default;

Result<std::unique_ptr<TlsTransport>> TlsTransport::Establish(std::unique_ptr<TcpTransport> tcp,
                                                              const TlsConfig& config) {
  if (config.hostname.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "tls: a hostname is required for certificate verification");
  }

  auto transport = std::unique_ptr<TlsTransport>(new TlsTransport());
  transport->impl_ = std::make_unique<Impl>();
  Impl& impl = *transport->impl_;
  impl.tcp = std::move(tcp);

  impl.context = SSL_CTX_new(TLS_client_method());
  if (impl.context == nullptr) {
    return Status::Error(ErrorKind::kTlsFailure, "tls: " + OpenSslError());
  }

  SSL_CTX_set_min_proto_version(impl.context, TLS1_2_VERSION);
  SSL_CTX_set_options(impl.context, SSL_OP_NO_COMPRESSION);
  // Verification is unconditional. There is no code path that clears this.
  SSL_CTX_set_verify(impl.context, SSL_VERIFY_PEER, nullptr);

  bool have_anchors = false;
  if (config.use_default_trust_store) {
    have_anchors = SSL_CTX_set_default_verify_paths(impl.context) == 1;
  }
  if (!config.trust_anchors_pem.empty()) {
    X509_STORE* store = SSL_CTX_get_cert_store(impl.context);
    for (const std::string& pem : config.trust_anchors_pem) {
      BIO* bio = BIO_new_mem_buf(pem.data(), static_cast<int>(pem.size()));
      if (bio == nullptr) continue;
      while (true) {
        X509* certificate = PEM_read_bio_X509(bio, nullptr, nullptr, nullptr);
        if (certificate == nullptr) break;
        if (X509_STORE_add_cert(store, certificate) == 1) have_anchors = true;
        X509_free(certificate);
      }
      BIO_free(bio);
    }
  }
  if (!have_anchors) {
    return Status::Error(ErrorKind::kTlsFailure,
                         "tls: no trust anchors are available to verify the server");
  }

  impl.ssl = SSL_new(impl.context);
  if (impl.ssl == nullptr) {
    return Status::Error(ErrorKind::kTlsFailure, "tls: " + OpenSslError());
  }

  // Hostname verification, both for SNI and for the certificate check.
  SSL_set_tlsext_host_name(impl.ssl, config.hostname.c_str());
  X509_VERIFY_PARAM* param = SSL_get0_param(impl.ssl);
  X509_VERIFY_PARAM_set_hostflags(param, X509_CHECK_FLAG_NO_PARTIAL_WILDCARDS);
  if (X509_VERIFY_PARAM_set1_host(param, config.hostname.c_str(), config.hostname.size()) != 1) {
    return Status::Error(ErrorKind::kTlsFailure, "tls: cannot set the expected hostname");
  }

  if (!config.alpn_protocol.empty()) {
    std::string wire;
    wire.push_back(static_cast<char>(config.alpn_protocol.size()));
    wire += config.alpn_protocol;
    SSL_set_alpn_protos(impl.ssl, reinterpret_cast<const unsigned char*>(wire.data()),
                        static_cast<unsigned int>(wire.size()));
  }

  SSL_set_fd(impl.ssl, impl.tcp->fd());

  Millis deadline = config.handshake_timeout_ms < 0
                        ? -1
                        : Clock::System()->MonotonicMillis() + config.handshake_timeout_ms;
  while (true) {
    int rc = SSL_connect(impl.ssl);
    if (rc == 1) break;
    int error = SSL_get_error(impl.ssl, rc);
    if (error == SSL_ERROR_WANT_READ || error == SSL_ERROR_WANT_WRITE) {
      Millis remaining = deadline < 0 ? -1 : deadline - Clock::System()->MonotonicMillis();
      Status wait = impl.tcp->Wait(error == SSL_ERROR_WANT_READ, remaining);
      if (!wait.ok()) return wait;
      continue;
    }
    long verify_result = SSL_get_verify_result(impl.ssl);
    if (verify_result != X509_V_OK) {
      return Status::Error(ErrorKind::kTlsFailure,
                           std::string("tls: certificate verification failed: ") +
                               X509_verify_cert_error_string(verify_result));
    }
    return Status::Error(ErrorKind::kTlsFailure, "tls: handshake failed: " + OpenSslError());
  }

  long verify_result = SSL_get_verify_result(impl.ssl);
  if (verify_result != X509_V_OK) {
    return Status::Error(ErrorKind::kTlsFailure,
                         std::string("tls: certificate verification failed: ") +
                             X509_verify_cert_error_string(verify_result));
  }

  TM_LOG_DEBUG(kTag, "tls established (%s)", SSL_get_version(impl.ssl));
  return transport;
}

Result<std::size_t> TlsTransport::Read(std::uint8_t* buffer, std::size_t size,
                                       Millis timeout_ms) {
  if (!impl_ || impl_->ssl == nullptr) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "tls session is closed");
  }
  Millis deadline = timeout_ms < 0 ? -1 : Clock::System()->MonotonicMillis() + timeout_ms;
  while (true) {
    ERR_clear_error();
    int rc = SSL_read(impl_->ssl, buffer, static_cast<int>(size));
    if (rc > 0) return static_cast<std::size_t>(rc);
    int error = SSL_get_error(impl_->ssl, rc);
    if (error == SSL_ERROR_ZERO_RETURN) return static_cast<std::size_t>(0);
    if (error == SSL_ERROR_WANT_READ || error == SSL_ERROR_WANT_WRITE) {
      Millis remaining = deadline < 0 ? -1 : deadline - Clock::System()->MonotonicMillis();
      Status wait = impl_->tcp->Wait(error == SSL_ERROR_WANT_READ, remaining);
      if (!wait.ok()) return wait;
      continue;
    }
    if (error == SSL_ERROR_SYSCALL && ERR_peek_error() == 0) {
      // Peer closed without a close_notify; treat as end of stream.
      return static_cast<std::size_t>(0);
    }
    return Status::Error(ErrorKind::kNetworkUnavailable, "tls read failed: " + OpenSslError());
  }
}

Status TlsTransport::WriteAll(ByteView data, Millis timeout_ms) {
  if (!impl_ || impl_->ssl == nullptr) {
    return Status::Error(ErrorKind::kNetworkUnavailable, "tls session is closed");
  }
  Millis deadline = timeout_ms < 0 ? -1 : Clock::System()->MonotonicMillis() + timeout_ms;
  std::size_t written = 0;
  while (written < data.size()) {
    ERR_clear_error();
    int rc = SSL_write(impl_->ssl, data.data() + written,
                       static_cast<int>(data.size() - written));
    if (rc > 0) {
      written += static_cast<std::size_t>(rc);
      continue;
    }
    int error = SSL_get_error(impl_->ssl, rc);
    if (error == SSL_ERROR_WANT_READ || error == SSL_ERROR_WANT_WRITE) {
      Millis remaining = deadline < 0 ? -1 : deadline - Clock::System()->MonotonicMillis();
      Status wait = impl_->tcp->Wait(error == SSL_ERROR_WANT_READ, remaining);
      if (!wait.ok()) return wait;
      continue;
    }
    return Status::Error(ErrorKind::kNetworkUnavailable, "tls write failed: " + OpenSslError());
  }
  return Status::Ok();
}

void TlsTransport::Close() {
  if (!impl_) return;
  if (impl_->ssl != nullptr) SSL_shutdown(impl_->ssl);
  if (impl_->tcp) impl_->tcp->Close();
}

void TlsTransport::Cancel() {
  if (impl_ && impl_->tcp) impl_->tcp->Cancel();
}

void TlsTransport::SetInterrupt(Notifier* notifier) {
  if (impl_ && impl_->tcp) impl_->tcp->SetInterrupt(notifier);
}

bool TlsTransport::is_open() const { return impl_ && impl_->tcp && impl_->tcp->is_open(); }

std::string TlsTransport::protocol_version() const {
  if (!impl_ || impl_->ssl == nullptr) return std::string();
  const char* version = SSL_get_version(impl_->ssl);
  return version == nullptr ? std::string() : std::string(version);
}

bool IsLoopbackHost(const std::string& host) {
  return host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" ||
         StartsWith(host, "127.");
}

Result<std::unique_ptr<Transport>> OpenTransport(const std::string& scheme,
                                                 const std::string& host, std::uint16_t port,
                                                 const TlsConfig& tls_config,
                                                 std::shared_ptr<CancelSignal> cancel,
                                                 const TransportOptions& options) {
  const bool secure = scheme == "https" || scheme == "wss";
  const bool tunnelled = options.dialer != nullptr;

  if (!secure) {
    if (tunnelled) {
      // The loopback exception must not be satisfied by accident here: a tunnel hands
      // back a descriptor, not a loopback address, and letting that count would turn
      // "cleartext stays on the device" into "cleartext crosses a network".
      if (!options.allow_cleartext_over_tunnel) {
        return Status::Error(ErrorKind::kTlsFailure,
                             "cleartext through a tunnel requires it to be enabled "
                             "explicitly; " + host + " should serve https");
      }
    } else if (!IsLoopbackHost(host)) {
      // spec §7.4: production connections are HTTPS/wss. A cleartext endpoint is only
      // ever reachable on loopback, where it cannot leave the device.
      return Status::Error(ErrorKind::kTlsFailure,
                           "cleartext transport is only permitted to a loopback host");
    }
  }

  auto tcp = std::make_unique<TcpTransport>(cancel);
  if (tunnelled) {
    if (!options.dialer->ready()) {
      return Status::Error(ErrorKind::kNetworkUnavailable,
                           options.dialer->name() + " is not ready");
    }
    Result<int> fd = options.dialer->DialFd(host, port, options.connect_timeout_ms);
    if (!fd.ok()) return fd.status();
    Status adopted = tcp->Adopt(fd.value());
    if (!adopted.ok()) return adopted;
  } else {
    Status connected = tcp->Connect(host, port, options.connect_timeout_ms);
    if (!connected.ok()) return connected;
  }

  if (!secure) {
    return std::unique_ptr<Transport>(tcp.release());
  }

  TlsConfig config = tls_config;
  if (config.hostname.empty()) config.hostname = host;
  Result<std::unique_ptr<TlsTransport>> tls = TlsTransport::Establish(std::move(tcp), config);
  if (!tls.ok()) return tls.status();
  return std::unique_ptr<Transport>(tls.take().release());
}

}  // namespace net
}  // namespace tmirror
