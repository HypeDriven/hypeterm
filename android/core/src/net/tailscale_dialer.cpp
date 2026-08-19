#include "tm/net/tailscale_dialer.h"

#include <dlfcn.h>

#include <cstdlib>
#include <vector>

#include "tm/util/json.h"
#include "tm/util/log.h"

namespace tmirror {
namespace net {
namespace {

constexpr const char kTag[] = "tm.tailscale";
constexpr const char kDefaultLibrary[] = "libhypeterm_tsnet.so";

/// Status and error documents are small and produced by our own library, but the
/// parser is bounded anyway: it costs nothing and keeps one code path for all JSON.
constexpr std::size_t kStatusBufferBytes = 8192;

/// Fills `out` from the node's status JSON. Separate from the accessor so the start
/// path can read it without re-entering the dialer's lock.
void ParseStatusDocument(const std::string& document, TailscaleStatus* out) {
  if (document.empty() || out == nullptr) return;
  JsonLimits limits;
  limits.max_bytes = 1 << 16;
  limits.max_depth = 8;
  limits.max_elements = 256;
  Result<Json> parsed = Json::Parse(document, limits);
  if (!parsed.ok()) return;

  const Json& json = parsed.value();
  out->started = json.GetBool("started", false);
  out->running = json.GetBool("running", false);
  out->backend_state = json.GetString("backend_state");
  out->auth_url = json.GetString("auth_url");
  out->hostname = json.GetString("hostname");
  out->no_log_upload = json.GetBool("no_log_upload", false);
  out->cache_dir = json.GetString("cache_dir");
  out->temp_dir = json.GetString("temp_dir");
  std::uint64_t peers = 0;
  if (json.GetUint64("peers", &peers) && peers <= 100000) {
    out->peers = static_cast<int>(peers);
  }
  const Json* addresses = json.Find("addresses");
  if (addresses != nullptr && addresses->is_array()) {
    for (const Json& address : addresses->items()) {
      if (address.is_string()) out->addresses.push_back(address.string_value());
      if (out->addresses.size() >= 8) break;
    }
  }
}

std::string* OverridePath() {
  static std::string* path = new std::string();
  return path;
}

}  // namespace

struct TailscaleDialer::Library {
  void* handle = nullptr;
  int (*start)(const char* state_dir, const char* hostname, const char* auth_key,
               const char* control_url, int verbose) = nullptr;
  int (*status)(char* buffer, int length) = nullptr;
  int (*dial)(const char* host, int port, int timeout_ms) = nullptr;
  void (*stop)() = nullptr;
  int (*logout)(const char* state_dir) = nullptr;
  int (*last_error)(char* buffer, int length) = nullptr;
  // Optional: a library built for a platform without the replacement enumeration will
  // not have it, and everything else still works.
  int (*interfaces)(char* buffer, int length) = nullptr;

  ~Library() {
    // The Go runtime cannot be unloaded safely, so the handle is deliberately kept
    // for the lifetime of the process even when the dialer goes away.
  }

  bool complete() const {
    return start && status && dial && stop && logout && last_error;
  }

  /// Reads one of the two string-returning entry points, which report the required
  /// size as a negative number when the buffer is too small.
  std::string ReadString(int (*fn)(char*, int)) const {
    std::vector<char> buffer(kStatusBufferBytes);
    int written = fn(buffer.data(), static_cast<int>(buffer.size()));
    if (written < 0) {
      std::size_t needed = static_cast<std::size_t>(-written);
      if (needed > 1u << 20) return std::string();
      buffer.assign(needed, 0);
      written = fn(buffer.data(), static_cast<int>(buffer.size()));
      if (written < 0) return std::string();
    }
    return std::string(buffer.data(), static_cast<std::size_t>(written));
  }
};

TailscaleDialer::TailscaleDialer(TailscaleConfig config) : config_(std::move(config)) {}

TailscaleDialer::~TailscaleDialer() {
  Stop();
}

void TailscaleDialer::SetLibraryPathForTesting(const std::string& path) {
  *OverridePath() = path;
}

std::shared_ptr<TailscaleDialer::Library> TailscaleDialer::Load() const {
  // Caller holds mutex_.
  if (load_attempted_) return library_;
  load_attempted_ = true;

  std::string path = *OverridePath();
  if (path.empty()) {
    const char* from_environment = std::getenv("HYPETERM_TSNET_LIB");
    path = from_environment != nullptr ? from_environment : kDefaultLibrary;
  }

  void* handle = ::dlopen(path.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (handle == nullptr) {
    // dlerror text names a library path, never a secret.
    const char* why = ::dlerror();
    TM_LOG_INFO(kTag, "the embedded Tailscale node is not present: %s",
                why != nullptr ? why : "unknown");
    return nullptr;
  }

  auto library = std::make_shared<Library>();
  library->handle = handle;
  library->start = reinterpret_cast<decltype(Library::start)>(
      ::dlsym(handle, "hypeterm_tsnet_start"));
  library->status = reinterpret_cast<decltype(Library::status)>(
      ::dlsym(handle, "hypeterm_tsnet_status"));
  library->dial = reinterpret_cast<decltype(Library::dial)>(
      ::dlsym(handle, "hypeterm_tsnet_dial"));
  library->stop = reinterpret_cast<decltype(Library::stop)>(
      ::dlsym(handle, "hypeterm_tsnet_stop"));
  library->logout = reinterpret_cast<decltype(Library::logout)>(
      ::dlsym(handle, "hypeterm_tsnet_logout"));
  library->last_error = reinterpret_cast<decltype(Library::last_error)>(
      ::dlsym(handle, "hypeterm_tsnet_last_error"));
  library->interfaces = reinterpret_cast<decltype(Library::interfaces)>(
      ::dlsym(handle, "hypeterm_tsnet_interfaces"));

  if (!library->complete()) {
    TM_LOG_WARN(kTag, "the Tailscale library is missing entry points; ignoring it");
    return nullptr;
  }
  library_ = std::move(library);
  return library_;
}

bool TailscaleDialer::available() const {
  std::lock_guard<std::mutex> lock(mutex_);
  return Load() != nullptr;
}

Status TailscaleDialer::Start(const std::string& auth_key) {
  // The library handle and the configuration are read under the lock; the call into Go
  // is made without it. Bringing a WireGuard node up takes the better part of a second,
  // and a logout can take ten — and `GetStatus` needs the same lock, so holding it
  // across the call freezes every status poll, which on Android is the UI thread.
  // The Go side serialises these entry points itself, so nothing here needs to.
  std::shared_ptr<Library> library;
  std::string state_dir;
  std::string hostname;
  std::string control_url;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (config_.state_dir.empty()) {
      return Status::Error(ErrorKind::kInvalidArgument,
                           "the Tailscale tunnel needs a private state directory");
    }
    library = Load();
    if (library == nullptr) {
      return Status::Error(ErrorKind::kNetworkUnavailable,
                           "this build does not include the Tailscale tunnel");
    }
    if (started_) return Status::Ok();
    state_dir = config_.state_dir;
    hostname = config_.hostname;
    control_url = config_.control_url;
  }

  // A debug build asks the node to explain itself; a release build never does, and
  // the node's lines can name peers (spec §9.3, §15).
#if defined(TM_DEBUG_BUILD)
  constexpr int kVerbose = 1;
#else
  constexpr int kVerbose = 0;
#endif
  int rc = library->start(state_dir.c_str(), hostname.c_str(), auth_key.c_str(),
                          control_url.c_str(), kVerbose);
  if (rc != 0) {
    std::string message = library->ReadString(library->last_error);
    // Worth logging: it is a configuration or platform problem the user cannot
    // diagnose from "not connected", and the text comes from the node, not the wire.
    TM_LOG_WARN(kTag, "the embedded Tailscale node did not start: %s",
                message.empty() ? "no reason given" : message.c_str());
    // Where the node may write is the usual culprit on Android, so name those two
    // directories — and only those. The status document also carries the login URL,
    // which authorises a node onto the tailnet and must never reach a log (spec §9.3,
    // §12, §15); dumping the whole document here would have put it there.
    TailscaleStatus storage;
    ParseStatusDocument(library->ReadString(library->status), &storage);
    TM_LOG_WARN(kTag, "node storage: cache=%s temp=%s",
                storage.cache_dir.empty() ? "(none)" : storage.cache_dir.c_str(),
                storage.temp_dir.empty() ? "(none)" : storage.temp_dir.c_str());
    return Status::Error(ErrorKind::kNetworkUnavailable,
                         message.empty() ? "the Tailscale tunnel failed to start"
                                         : message);
  }
  {
    std::lock_guard<std::mutex> lock(mutex_);
    started_ = true;
  }
  TM_LOG_INFO(kTag, "the embedded Tailscale node is starting");
  return Status::Ok();
}

void TailscaleDialer::Stop() {
  std::shared_ptr<Library> library;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    if (!started_ || library_ == nullptr) {
      started_ = false;
      return;
    }
    library = library_;
    started_ = false;
  }
  // Outside the lock, for the reason in Start().
  library->stop();
}

Status TailscaleDialer::Logout() {
  std::shared_ptr<Library> library;
  std::string state_dir;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    library = Load();
    if (library == nullptr) return Status::Ok();
    state_dir = config_.state_dir;
    started_ = false;
  }
  // The slowest of the three, and the one that made the tunnel screen stop repainting
  // for ten seconds while it ran.
  int rc = library->logout(state_dir.c_str());
  if (rc != 0) {
    std::string message = library->ReadString(library->last_error);
    return Status::Error(ErrorKind::kInternal,
                         message.empty() ? "the Tailscale tunnel could not be reset"
                                         : message);
  }
  return Status::Ok();
}

TailscaleStatus TailscaleDialer::GetStatus() const {
  TailscaleStatus result;
  std::lock_guard<std::mutex> lock(mutex_);
  std::shared_ptr<Library> library = Load();
  if (library == nullptr) {
    result.backend_state = "Unavailable";
    return result;
  }
  result.available = true;
  result.last_error = library->ReadString(library->last_error);
  ParseStatusDocument(library->ReadString(library->status), &result);

  // A node that is coming up, waiting to be authorised, or has dropped out is the
  // difference between "the relay is unreachable" and "you have not signed in yet", so
  // say so once per transition. The login URL is a capability and stays out of release
  // logs; the state name and node name are neither secret nor payload.
  if (result.backend_state != last_backend_state_) {
    last_backend_state_ = result.backend_state;
    TM_LOG_INFO(kTag, "node state: %s%s%s", result.backend_state.c_str(),
                result.hostname.empty() ? "" : " as ", result.hostname.c_str());
  }
  // The URL arrives after the state changes, and it is the thing the user has to act
  // on, so it is announced on its own. The URL itself is a capability and stays out of
  // release logs.
  const bool waiting = !result.auth_url.empty();
  if (waiting != announced_auth_url_) {
    announced_auth_url_ = waiting;
    if (waiting) {
      TM_LOG_INFO(kTag, "the node is waiting to be authorised in a browser");
      TM_LOG_PAYLOAD(kTag, "authorise at %s", result.auth_url.c_str());
    }
  }
  return result;
}

Result<std::string> TailscaleDialer::InterfacesJson() const {
  std::lock_guard<std::mutex> lock(mutex_);
  std::shared_ptr<Library> library = Load();
  if (library == nullptr || library->interfaces == nullptr) {
    return Status::Error(ErrorKind::kNetworkUnavailable,
                         "this build cannot enumerate interfaces");
  }
  std::string document = library->ReadString(library->interfaces);
  if (document.empty()) {
    std::string message = library->ReadString(library->last_error);
    return Status::Error(ErrorKind::kInternal,
                         message.empty() ? "interfaces could not be listed" : message);
  }
  return document;
}

bool TailscaleDialer::ready() const {
  return GetStatus().running;
}

std::string TailscaleDialer::name() const {
  // This ends up in the message the user reads when a connection is refused, so it
  // says which of the several "not ready" situations they are in. "Not ready" alone
  // leaves them with nothing to do.
  TailscaleStatus status = GetStatus();
  if (!status.available) return "the Tailscale tunnel (not included in this build)";
  if (!status.started) return "the Tailscale tunnel (not started)";
  if (status.running) return "the Tailscale tunnel";
  if (!status.auth_url.empty()) {
    return "the Tailscale tunnel (open Connection settings to finish signing in)";
  }
  return "the Tailscale tunnel (still connecting)";
}

Result<int> TailscaleDialer::DialFd(const std::string& host, std::uint16_t port,
                                    Millis timeout_ms) {
  std::shared_ptr<Library> library;
  {
    std::lock_guard<std::mutex> lock(mutex_);
    library = Load();
    if (library == nullptr || !started_) {
      return Status::Error(ErrorKind::kNetworkUnavailable,
                           "the Tailscale tunnel is not running");
    }
  }

  // Dialling is a blocking network operation; it must not hold the lock, or a status
  // poll from the UI thread would stall behind it.
  int fd = library->dial(host.c_str(), static_cast<int>(port),
                         static_cast<int>(timeout_ms));
  if (fd < 0) {
    std::string message = library->ReadString(library->last_error);
    return Status::Error(ErrorKind::kNetworkUnavailable,
                         message.empty() ? "the Tailscale tunnel could not reach the relay"
                                         : message);
  }
  return fd;
}

}  // namespace net
}  // namespace tmirror
