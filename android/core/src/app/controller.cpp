#include "tm/app/controller.h"

#include "tm/api/pairing.h"

#include <algorithm>

#include "tm/input/paste.h"
#include "tm/util/base64.h"
#include "tm/util/log.h"
#include "tm/util/random.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace app {
namespace {

constexpr const char kTag[] = "tm.controller";
constexpr Millis kPingIntervalMs = 20000;
constexpr Millis kPersistIntervalMs = 5000;

}  // namespace

const char* ConnectionStateName(ConnectionState state) {
  switch (state) {
    case ConnectionState::kIdle: return "idle";
    case ConnectionState::kPairingRequired: return "pairing_required";
    case ConnectionState::kAuthenticating: return "authenticating";
    case ConnectionState::kDiscovering: return "discovering";
    case ConnectionState::kAttaching: return "attaching";
    case ConnectionState::kAttached: return "attached";
    case ConnectionState::kReconnecting: return "reconnecting";
    case ConnectionState::kTerminalClosed: return "terminal_closed";
    case ConnectionState::kFailed: return "failed";
    case ConnectionState::kStopped: return "stopped";
  }
  return "unknown";
}

Controller::Controller(AppConfig config, api::SecureStore* secure_store,
                       Preferences* preferences, ControllerCallbacks callbacks, Clock* clock)
    : config_(std::move(config)),
      secure_store_(secure_store),
      preferences_(preferences),
      callbacks_(std::move(callbacks)),
      clock_(clock),
      credential_store_(secure_store),
      backoff_(config_.backoff),
      cancel_(std::make_shared<net::CancelSignal>()),
      commands_(config_.command_queue_depth, config_.pending_input_bytes),
      duplicate_filter_(clock) {
  local_columns_ = config_.fallback_columns;
  local_rows_ = config_.fallback_rows;
}

Controller::~Controller() { Stop(); }

Status Controller::Start() {
  Status valid = config_.Validate();
  if (!valid.ok()) return valid;
  if (running_.exchange(true)) return Status::Ok();

  Result<net::Url> base = net::ParseUrl(config_.server_url);
  if (!base.ok()) {
    running_.store(false);
    return base.status();
  }

  api::RelayClientConfig relay_config;
  relay_config.base_url = base.value();
  relay_config.tls = config_.tls;
  relay_config.connect_timeout_ms = config_.connect_timeout_ms;
  relay_config.request_timeout_ms = config_.request_timeout_ms;
  relay_config.dialer = config_.dialer;
  relay_config.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;
  relay_ = std::make_unique<api::RelayClient>(relay_config);
  relay_->set_cancel_signal(cancel_);

  session_ = std::make_unique<TerminalSession>(config_);
  session_->emulator().SetTitleCallback([this](const std::string& title) {
    if (callbacks_.on_title) callbacks_.on_title(title);
  });
  session_->emulator().SetBellCallback([this]() {
    if (callbacks_.on_bell) callbacks_.on_bell();
  });
  session_->emulator().SetClipboardCallback([this](const std::string& text) {
    if (callbacks_.on_clipboard_write) callbacks_.on_clipboard_write(text);
  });
  session_->emulator().SetResponseSink([this](ByteView bytes) {
    // Terminal replies (DA, DSR, CPR) travel as terminal input. A read-only
    // attachment silently drops them: they are answers to the remote's own query,
    // not user input, so there is nothing to report (relay spec §4.5).
    if (mirror_ && mirror_->input_available()) {
      SendInputBytes(bytes.to_string(), false);
    }
  });

  bool paired = false;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    EnsureCredentialsLoaded();
    paired = credentials_ && credentials_->complete();
  }
  SetState(paired ? ConnectionState::kIdle : ConnectionState::kPairingRequired);

  commands_.Reopen();
  thread_ = std::thread([this] { NetworkThreadMain(); });
  return Status::Ok();
}

Status Controller::SetServerUrl(const std::string& url) {
  Result<net::Url> parsed = net::ParseUrl(url);
  if (!parsed.ok()) return parsed.status();
  if (running_.load()) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "the relay URL cannot change while the client is running");
  }
  std::lock_guard<std::mutex> lock(state_mutex_);
  config_.server_url = url;
  // A pairing already in progress keeps the key it generated but adopts the new URL,
  // so the credential that gets saved names the relay it was paired against.
  if (credentials_) credentials_->server_url = url;
  return Status::Ok();
}

std::string Controller::server_url() const {
  std::lock_guard<std::mutex> lock(state_mutex_);
  return config_.server_url;
}

void Controller::Stop() {
  if (!running_.exchange(false)) return;
  Command command;
  command.type = Command::Type::kShutdown;
  commands_.Push(std::move(command));
  notifier_.Notify();
  cancel_->Cancel();
  commands_.Close();
  if (thread_.joinable()) thread_.join();
  SetState(ConnectionState::kStopped);
}

// ------------------------------------------------------------------- pairing

Result<PairingInfo> Controller::BeginPairing() {
  Result<api::DeviceCredentials> generated =
      api::CredentialStore::GenerateNew(config_.server_url, config_.device_name);
  if (!generated.ok()) return generated.status();

  PairingInfo info;
  info.public_key_base64url = generated.value().public_key_base64url;
  info.key_fingerprint = generated.value().key_fingerprint;
  info.server_url = generated.value().server_url;
  info.device_name = generated.value().device_name;

  std::lock_guard<std::mutex> lock(state_mutex_);
  credentials_ = std::make_unique<api::DeviceCredentials>(std::move(generated.value()));
  return info;
}

Status Controller::CompletePairing(const std::string& identity_id,
                                   const std::string& device_id) {
  std::lock_guard<std::mutex> lock(state_mutex_);
  if (!credentials_) {
    return Status::Error(ErrorKind::kInvalidArgument, "no pairing is in progress");
  }
  if (identity_id.empty() || device_id.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument, "identity and device ids are required");
  }
  credentials_->identity_id = identity_id;
  credentials_->device_id = device_id;
  if (credentials_->server_url.empty()) credentials_->server_url = config_.server_url;
  if (credentials_->server_url.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument,
                         "set the relay URL before finishing pairing");
  }
  Status status = credential_store_.Save(*credentials_);
  if (!status.ok()) return status;
  status_.state = ConnectionState::kIdle;
  return Status::Ok();
}

Result<std::string> Controller::CompletePairingWithCode(const std::string& code) {
  Result<api::PairingCode> decoded = api::DecodePairingCode(code);
  if (!decoded.ok()) return decoded.status();

  // The device key generated by BeginPairing, or a fresh one if the screen was
  // reached without it. Its private half never leaves this device.
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    if (!credentials_) {
      Result<api::DeviceCredentials> generated = api::CredentialStore::GenerateNew(
          decoded.value().server_url, config_.device_name);
      if (!generated.ok()) return generated.status();
      credentials_ = std::make_unique<api::DeviceCredentials>(std::move(generated.value()));
    }
  }

  // A client built for the code's own URL rather than the configured one: pairing may
  // well be the moment the relay's address is learned, and the running controller's
  // client is bound to the old origin.
  Result<net::Url> base = net::ParseUrl(decoded.value().server_url);
  if (!base.ok()) return base.status();

  api::RelayClientConfig relay_config;
  relay_config.base_url = base.value();
  relay_config.tls = config_.tls;
  relay_config.connect_timeout_ms = config_.connect_timeout_ms;
  relay_config.request_timeout_ms = config_.request_timeout_ms;
  relay_config.dialer = config_.dialer;
  relay_config.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;
  api::RelayClient client(relay_config);
  client.set_cancel_signal(cancel_);

  Bytes public_key;
  std::string key_fingerprint;
  crypto::Ed25519KeyPair key;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    Result<crypto::Ed25519KeyPair> restored =
        crypto::Ed25519KeyPair::FromSeed(ByteView(credentials_->private_key_seed));
    if (!restored.ok()) return restored.status();
    key = std::move(restored.value());
    public_key = key.public_key();
    key_fingerprint = credentials_->key_fingerprint;
  }

  Result<api::Challenge> challenge = client.CreateDeviceRegistrationChallenge(
      crypto::kAlgorithmEd25519, ByteView(public_key), decoded.value().identity_id);
  if (!challenge.ok()) return challenge.status();

  crypto::ExpectedSigningInput expected;
  expected.challenge_id = challenge.value().challenge_id;
  expected.challenge = challenge.value().challenge;
  expected.operation = crypto::ChallengeOperation::kRegisterDevice;
  expected.key_fingerprint = key_fingerprint;
  expected.owner_identity_id = decoded.value().identity_id;
  expected.device_key_fingerprint = key_fingerprint;
  expected.expected_origin = base.value().origin();
  // A relay that asks this device to sign a registration binding it to some other
  // identity is not getting a signature (spec §12).
  Status verified =
      crypto::VerifySigningInput(ByteView(challenge.value().signing_input), expected);
  if (!verified.ok()) {
    // The origin is the one field a legitimate deployment may differ on — a proxy in
    // front of the relay, or a different public host name.
    expected.expected_origin.clear();
    verified = crypto::VerifySigningInput(ByteView(challenge.value().signing_input), expected);
    if (!verified.ok()) return verified;
  }

  Result<Bytes> signature = key.Sign(ByteView(challenge.value().signing_input));
  if (!signature.ok()) return signature.status();

  api::AccessToken borrowed;
  borrowed.token = decoded.value().identity_token;

  Result<api::DeviceInfo> registered = client.RegisterDevice(
      borrowed, config_.device_name, crypto::kAlgorithmEd25519, ByteView(public_key),
      challenge.value().challenge_id, ByteView(signature.value()),
      // A phone mirrors and types; it never publishes (relay spec §3.2).
      "client");
  if (!registered.ok()) return registered.status();

  std::lock_guard<std::mutex> lock(state_mutex_);
  credentials_->server_url = decoded.value().server_url;
  credentials_->identity_id = decoded.value().identity_id;
  credentials_->device_id = registered.value().device_id;
  Status saved = credential_store_.Save(*credentials_);
  if (!saved.ok()) return saved;
  status_.state = ConnectionState::kIdle;
  TM_LOG_INFO(kTag, "paired with the relay as a client device");
  return decoded.value().server_url;
}

namespace {

/// A fresh idempotency key.
///
/// One per user action, and reused verbatim by a retry of that same action: that is
/// what turns an ambiguous timeout into a question the relay can answer, instead of a
/// second shell on somebody's machine (relay spec §5.2).
std::string NewIdempotencyKey() {
  return Base64UrlEncode(ByteView(SecureRandomBytes(16)));
}

}  // namespace

Result<api::TerminalInfo> Controller::OpenTerminal(const std::string& device_id,
                                                   const std::string& label, int columns,
                                                   int rows) {
  if (device_id.empty()) {
    return Status::Error(ErrorKind::kInvalidArgument, "no machine was chosen");
  }
  Status token = EnsureToken();
  if (!token.ok()) return token;

  api::AccessToken access;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    access = token_;
  }
  // Its own client rather than the network thread's: this waits on a far machine
  // starting a process, and the shared one is servicing the mirror's read loop.
  api::RelayClientConfig relay_config;
  Result<net::Url> base = net::ParseUrl(server_url());
  if (!base.ok()) return base.status();
  relay_config.base_url = base.value();
  relay_config.tls = config_.tls;
  relay_config.connect_timeout_ms = config_.connect_timeout_ms;
  relay_config.request_timeout_ms = config_.request_timeout_ms;
  relay_config.dialer = config_.dialer;
  relay_config.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;
  api::RelayClient client(relay_config);
  client.set_cancel_signal(cancel_);

  return client.OpenTerminal(access, device_id, label, columns, rows, NewIdempotencyKey());
}

Result<std::vector<api::DeviceInfo>> Controller::ListDevices() {
  Status token = EnsureToken();
  if (!token.ok()) return token;

  api::AccessToken access;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    access = token_;
  }
  api::RelayClientConfig relay_config;
  Result<net::Url> base = net::ParseUrl(server_url());
  if (!base.ok()) return base.status();
  relay_config.base_url = base.value();
  relay_config.tls = config_.tls;
  relay_config.connect_timeout_ms = config_.connect_timeout_ms;
  relay_config.request_timeout_ms = config_.request_timeout_ms;
  relay_config.dialer = config_.dialer;
  relay_config.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;
  api::RelayClient client(relay_config);
  client.set_cancel_signal(cancel_);
  return client.ListDevices(access);
}

Status Controller::ForgetCredentials() {
  std::lock_guard<std::mutex> lock(state_mutex_);
  credentials_.reset();
  token_ = api::AccessToken();
  status_.state = ConnectionState::kPairingRequired;
  return credential_store_.Clear();
}

void Controller::EnsureCredentialsLoaded() {
  if (credentials_ || credentials_load_attempted_) return;
  credentials_load_attempted_ = true;
  Result<api::DeviceCredentials> loaded = credential_store_.Load();
  if (loaded.ok()) {
    credentials_ = std::make_unique<api::DeviceCredentials>(std::move(loaded.value()));
  }
}

bool Controller::HasCredentials() {
  // Asked before Start() on every cold launch, so the store is consulted here rather
  // than only during startup.
  std::lock_guard<std::mutex> lock(state_mutex_);
  EnsureCredentialsLoaded();
  return credentials_ && credentials_->complete();
}

// ------------------------------------------------------------------ commands

bool Controller::Post(Command command, std::size_t weight) {
  if (!running_.load()) return false;
  PushResult result = commands_.Push(std::move(command), weight);
  if (result != PushResult::kOk) {
    // A full queue is reported, never silently dropped (spec §6.2, §9.3).
    Notify(ErrorKind::kInternal, "the client is busy; that action was not queued");
    return false;
  }
  notifier_.Notify();
  return true;
}

bool Controller::RefreshTerminals() {
  Command command;
  command.type = Command::Type::kRefreshTerminals;
  return Post(std::move(command));
}

bool Controller::Attach(const std::string& terminal_id) {
  Command command;
  command.type = Command::Type::kAttach;
  command.text = terminal_id;
  return Post(std::move(command));
}

bool Controller::Detach() {
  Command command;
  command.type = Command::Type::kDetach;
  return Post(std::move(command));
}

bool Controller::SendKey(const input::KeyEvent& event) {
  Command command;
  command.type = Command::Type::kKey;
  command.key = event;
  return Post(std::move(command), 16);
}

bool Controller::SendText(const std::string& utf8_text) {
  if (utf8_text.empty()) return true;
  Command command;
  command.type = Command::Type::kText;
  command.text = utf8_text;
  return Post(std::move(command), utf8_text.size());
}

bool Controller::Paste(const std::string& utf8_text) {
  if (utf8_text.empty()) return true;
  Command command;
  command.type = Command::Type::kPaste;
  command.text = utf8_text;
  return Post(std::move(command), utf8_text.size());
}

bool Controller::SendMouse(const input::MouseEvent& event) {
  Command command;
  command.type = Command::Type::kMouse;
  command.mouse = event;
  return Post(std::move(command), 16);
}

bool Controller::SetFocused(bool focused) {
  Command command;
  command.type = Command::Type::kFocus;
  command.flag = focused;
  return Post(std::move(command));
}

bool Controller::SetGridSize(int columns, int rows) {
  if (columns < 1 || rows < 1) return false;
  Command command;
  command.type = Command::Type::kResize;
  command.a = columns;
  command.b = rows;
  return Post(std::move(command));
}

bool Controller::ScrollLines(int delta) {
  Command command;
  command.type = Command::Type::kScroll;
  command.a = delta;
  return Post(std::move(command));
}

bool Controller::ScrollToBottom() {
  Command command;
  command.type = Command::Type::kScrollToBottom;
  return Post(std::move(command));
}

bool Controller::SetSelection(const term::Selection& selection) {
  Command command;
  command.type = Command::Type::kSelection;
  command.selection = selection;
  return Post(std::move(command));
}

bool Controller::ClearSelection() {
  Command command;
  command.type = Command::Type::kClearSelection;
  return Post(std::move(command));
}

bool Controller::SetPaused(bool paused) {
  Command command;
  command.type = Command::Type::kPause;
  command.flag = paused;
  return Post(std::move(command));
}

bool Controller::SetNetworkAvailable(bool available) {
  Command command;
  command.type = Command::Type::kNetwork;
  command.flag = available;
  return Post(std::move(command));
}

// ------------------------------------------------------------------- reading

term::SnapshotRef Controller::LatestSnapshot() const {
  return session_ ? session_->latest_snapshot() : term::SnapshotRef();
}

SessionStatus Controller::status() const {
  std::lock_guard<std::mutex> lock(state_mutex_);
  return status_;
}

std::string Controller::SelectedText() const {
  term::SnapshotRef snapshot = LatestSnapshot();
  return snapshot ? ExtractSelection(*snapshot) : std::string();
}

std::string Controller::VisibleText() const {
  term::SnapshotRef snapshot = LatestSnapshot();
  return snapshot ? ExtractVisibleText(*snapshot) : std::string();
}

// ------------------------------------------------------------- network thread

void Controller::NetworkThreadMain() {
  TM_LOG_INFO(kTag, "network thread started");
  while (running_.load()) {
    ProcessCommands();
    if (!running_.load()) break;

    if (!desired_terminal_id_.empty() && !attached_ && !paused_ && network_available_) {
      Millis now = clock_->MonotonicMillis();
      if (now >= next_attempt_ms_) MaybeConnect();
    }

    if (attached_) {
      PumpSession();
      CheckHeartbeat();
      MaybeSendResize();
      MaybePersistResume(false);
    } else {
      // Nothing attached: sleep until a command arrives or the next retry is due.
      Millis now = clock_->MonotonicMillis();
      Millis wait = 1000;
      if (!desired_terminal_id_.empty() && !paused_ && network_available_) {
        wait = std::max<Millis>(10, next_attempt_ms_ - now);
      }
      Command command;
      if (commands_.Pop(&command, wait)) {
        HandleCommand(command);
      }
    }

    if (paused_ && attached_ && config_.detach_when_backgrounded && paused_since_ms_ >= 0 &&
        clock_->MonotonicMillis() - paused_since_ms_ >= config_.background_grace_ms) {
      // spec §11: keep the connection only while it is allowed and useful.
      Disconnect(Status::Error(ErrorKind::kCancelled, "detached while in the background"), false);
      Notify(ErrorKind::kNone, "detached while in the background");
    }

    MaybePublishSnapshot(false);
  }

  // Disconnect persists the resume cursor on its way out.
  Disconnect(Status::Error(ErrorKind::kCancelled, "client stopped"), false);
  TM_LOG_INFO(kTag, "network thread stopped");
}

void Controller::ProcessCommands() {
  Command command;
  int processed = 0;
  while (commands_.TryPop(&command)) {
    HandleCommand(command);
    if (++processed > 256) break;  // let the read loop run again
  }
}

void Controller::HandleCommand(Command& command) {
  switch (command.type) {
    case Command::Type::kShutdown:
      running_.store(false);
      return;

    case Command::Type::kRefreshTerminals: {
      Status token = EnsureToken();
      if (!token.ok()) {
        SetState(ConnectionState::kFailed, token);
        Notify(token.kind(), token.message());
        return;
      }
      SetState(ConnectionState::kDiscovering);
      Result<api::TerminalPage> page = relay_->ListTerminals(token_, "open", std::string(), 100);
      if (!page.ok()) {
        SetState(ConnectionState::kFailed, page.status());
        Notify(page.status().kind(), page.status().message());
        return;
      }
      if (callbacks_.on_terminals) callbacks_.on_terminals(page.value().terminals);
      SetState(attached_ ? ConnectionState::kAttached : ConnectionState::kIdle);
      return;
    }

    case Command::Type::kAttach: {
      if (command.text.empty()) return;
      if (attached_ && command.text == desired_terminal_id_) return;
      // A new attachment supersedes the old one; the generation bump makes any late
      // callback from the previous connection inert (spec §11).
      Disconnect(Status::Error(ErrorKind::kCancelled, "attaching elsewhere"), false);
      desired_terminal_id_ = command.text;
      cold_attach_ = true;
      resume_offset_valid_ = false;
      resume_offset_ = 0;
      session_->ResetTerminal();
      backoff_.Reset();
      next_attempt_ms_ = clock_->MonotonicMillis();
      SetState(ConnectionState::kAttaching);
      return;
    }

    case Command::Type::kDetach:
      // Disconnect first: it persists the resume cursor, which needs both the session
      // and the terminal id still in place.
      Disconnect(Status::Error(ErrorKind::kCancelled, "detached by the user"), false);
      desired_terminal_id_.clear();
      SetState(ConnectionState::kIdle);
      return;

    case Command::Type::kKey: {
      std::string bytes;
      input::KeyboardModes modes = session_->keyboard_modes();
      if (!input::KeyEncoder::EncodeKey(command.key, modes, &bytes)) return;
      duplicate_filter_.RecordKeyBytes(bytes);
      SendInputBytes(bytes, true);
      return;
    }

    case Command::Type::kText: {
      // Android delivers a key event and an IME commit for the same character often
      // enough that sending both would double it (spec §9.2).
      input::KeyboardModes modes = session_->keyboard_modes();
      std::string bytes = input::KeyEncoder::EncodeText(command.text, modes);
      if (duplicate_filter_.ShouldSuppressText(bytes)) return;
      SendInputBytes(bytes, true);
      return;
    }

    case Command::Type::kPaste: {
      input::Paste::Options options;
      options.bracketed = session_->emulator().bracketed_paste();
      options.chunk_bytes = config_.paste_chunk_bytes;
      options.max_bytes = config_.paste_max_bytes;
      bool too_large = false;
      std::vector<std::string> chunks = input::Paste::Prepare(command.text, options, &too_large);
      if (too_large) {
        Notify(ErrorKind::kInvalidArgument, "that paste is too large to send");
        return;
      }
      for (const std::string& chunk : chunks) SendInputBytes(chunk, true);
      return;
    }

    case Command::Type::kMouse: {
      std::string bytes;
      if (!input::EncodeMouseEvent(command.mouse, session_->emulator().mouse_tracking(),
                                   session_->emulator().mouse_encoding(), &bytes)) {
        return;
      }
      SendInputBytes(bytes, true);
      return;
    }

    case Command::Type::kFocus: {
      focused_ = command.flag;
      std::string bytes =
          input::KeyEncoder::EncodeFocus(focused_, session_->emulator().focus_reporting());
      if (!bytes.empty()) SendInputBytes(bytes, false);
      return;
    }

    case Command::Type::kResize: {
      local_columns_ = command.a;
      local_rows_ = command.b;
      if (command.a == pending_columns_ && command.b == pending_rows_) return;
      pending_columns_ = command.a;
      pending_rows_ = command.b;
      // Debounced so a rotation or an interactive layout change does not produce a
      // storm, while the final dimensions are always sent (spec §10.3).
      pending_resize_ms_ = clock_->MonotonicMillis() + config_.resize_debounce_ms;
      // Until the publisher answers, render at the size we have; if the relay never
      // reported one, adopt the local grid immediately so the screen is usable.
      if (session_->emulator().columns() == config_.fallback_columns &&
          session_->emulator().rows() == config_.fallback_rows && !attached_) {
        session_->ResizeGrid(command.a, command.b);
      }
      return;
    }

    case Command::Type::kScroll: {
      // While a full-screen application is tracking the mouse, a scroll gesture is a
      // wheel event for that application rather than local scrollback movement —
      // which is what makes a wheel work in `less` and `vim`.
      if (session_->emulator().mouse_tracking() != term::MouseTracking::kOff &&
          session_->emulator().alt_screen_active()) {
        input::MouseEvent wheel;
        wheel.action = input::MouseAction::kPress;
        wheel.button = command.a > 0 ? input::MouseButton::kWheelUp
                                     : input::MouseButton::kWheelDown;
        wheel.column = 0;
        wheel.row = 0;
        int steps = command.a < 0 ? -command.a : command.a;
        if (steps > 16) steps = 16;
        for (int i = 0; i < steps; ++i) {
          std::string bytes;
          if (input::EncodeMouseEvent(wheel, session_->emulator().mouse_tracking(),
                                      session_->emulator().mouse_encoding(), &bytes)) {
            SendInputBytes(bytes, true);
          }
        }
        return;
      }
      session_->ScrollLines(command.a);
      MaybePublishSnapshot(true);
      return;
    }

    case Command::Type::kScrollToBottom:
      session_->ScrollToBottom();
      MaybePublishSnapshot(true);
      return;

    case Command::Type::kSelection:
      session_->SetSelection(command.selection);
      MaybePublishSnapshot(true);
      return;

    case Command::Type::kClearSelection:
      session_->ClearSelection();
      MaybePublishSnapshot(true);
      return;

    case Command::Type::kPause:
      paused_ = command.flag;
      paused_since_ms_ = paused_ ? clock_->MonotonicMillis() : -1;
      if (!paused_) {
        // Resume: retry immediately rather than waiting out the backoff.
        backoff_.Reset();
        next_attempt_ms_ = clock_->MonotonicMillis();
      }
      return;

    case Command::Type::kNetwork:
      network_available_ = command.flag;
      {
        std::lock_guard<std::mutex> lock(state_mutex_);
        status_.network_available = network_available_;
      }
      if (!network_available_ && attached_) {
        Disconnect(Status::Error(ErrorKind::kNetworkUnavailable, "network unavailable"), true);
      } else if (network_available_) {
        backoff_.Reset();
        next_attempt_ms_ = clock_->MonotonicMillis();
      }
      PublishStatus();
      return;
  }
}

Status Controller::EnsureToken() {
  std::unique_ptr<api::DeviceCredentials> credentials;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    if (!credentials_ || !credentials_->complete()) {
      return Status::Error(ErrorKind::kAuthFailed, "this device is not paired yet");
    }
    // Copy only what is needed; the seed stays in the original object.
    credentials = std::make_unique<api::DeviceCredentials>();
    credentials->server_url = credentials_->server_url;
    credentials->identity_id = credentials_->identity_id;
    credentials->device_id = credentials_->device_id;
    credentials->private_key_seed = credentials_->private_key_seed;
    credentials->public_key_base64url = credentials_->public_key_base64url;
  }

  if (!token_.NeedsRefresh(clock_->UnixMillis())) return Status::Ok();

  SetState(ConnectionState::kAuthenticating);
  Result<crypto::Ed25519KeyPair> key = credentials->LoadKeyPair();
  if (!key.ok()) return key.status();

  // There are no refresh tokens: re-authenticating with the device key *is* the
  // refresh, and it needs no user interaction (relay reconciliation §2.2).
  Result<api::AccessToken> token = relay_->AuthenticateDevice(key.value());
  if (!token.ok()) {
    if (token.status().kind() == ErrorKind::kAuthFailed) {
      // A revoked or unknown device cannot recover by retrying with the same key.
      SecureZero(token_.token);
      token_ = api::AccessToken();
    }
    return token.status();
  }
  // The replaced token is cleared rather than left in a freed allocation (spec §12).
  SecureZero(token_.token);
  token_ = token.take();
  if (!token_.HasScope("terminals:mirror")) {
    return Status::Error(ErrorKind::kPermissionDenied,
                         "this device may not mirror terminals");
  }
  return Status::Ok();
}

void Controller::MaybeConnect() {
  const std::uint64_t generation = ++generation_;
  SetState(ConnectionState::kAttaching);

  Status token = EnsureToken();
  if (!token.ok()) {
    Millis delay = backoff_.NextDelay();
    next_attempt_ms_ = clock_->MonotonicMillis() + delay;
    SetState(token.kind() == ErrorKind::kAuthFailed ? ConnectionState::kFailed
                                                    : ConnectionState::kReconnecting,
             token);
    Notify(token.kind(), token.message());
    return;
  }

  if (cold_attach_) {
    // Confirm the terminal exists and is ours before upgrading; a 404 here is an
    // ownership answer as much as an existence one (relay spec §4.4).
    Result<api::TerminalInfo> info = relay_->GetTerminal(token_, desired_terminal_id_);
    if (!info.ok()) {
      Millis delay = backoff_.NextDelay();
      next_attempt_ms_ = clock_->MonotonicMillis() + delay;
      SetState(info.status().kind() == ErrorKind::kNotFound ? ConnectionState::kFailed
                                                            : ConnectionState::kReconnecting,
               info.status());
      Notify(info.status().kind(), info.status().message());
      if (info.status().kind() == ErrorKind::kNotFound) desired_terminal_id_.clear();
      return;
    }
    {
      std::lock_guard<std::mutex> lock(state_mutex_);
      status_.terminal_label = info.value().label;
    }
    if (!info.value().open()) {
      Notify(ErrorKind::kTerminalClosed, "that terminal has ended; showing its final output");
    }
  }

  api::MirrorSessionConfig mirror_config;
  mirror_config.base_url = relay_->config().base_url;
  mirror_config.tls = config_.tls;
  mirror_config.terminal_id = desired_terminal_id_;
  mirror_config.connect_timeout_ms = config_.connect_timeout_ms;
  mirror_config.user_agent = config_.user_agent;
  mirror_config.interrupt = &notifier_;
  mirror_config.dialer = config_.dialer;
  mirror_config.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;

  mirror_ = std::make_unique<api::MirrorSession>(
      mirror_config, [this, generation](const api::MirrorEvent& event) {
        HandleMirrorEvent(event, generation);
      });

  Status connected = mirror_->Connect(token_.token, std::string(), cancel_);
  if (!connected.ok()) {
    mirror_.reset();
    Millis delay = backoff_.NextDelay();
    next_attempt_ms_ = clock_->MonotonicMillis() + delay;
    bool fatal = connected.kind() == ErrorKind::kNotFound ||
                 connected.kind() == ErrorKind::kProtocolIncompatible;
    SetState(fatal ? ConnectionState::kFailed : ConnectionState::kReconnecting, connected);
    Notify(connected.kind(), connected.message());
    if (connected.kind() == ErrorKind::kAuthFailed) token_ = api::AccessToken();
    return;
  }

  // A cold attach replays the whole retained window so the screen is rebuilt from
  // authoritative bytes; a warm reconnect resumes exactly where it stopped
  // (spec §7.3, relay reconciliation §2.7).
  Status subscribed = mirror_->Subscribe(resume_offset_valid_, resume_offset_);
  if (!subscribed.ok()) {
    mirror_.reset();
    Millis delay = backoff_.NextDelay();
    next_attempt_ms_ = clock_->MonotonicMillis() + delay;
    SetState(ConnectionState::kReconnecting, subscribed);
    return;
  }

  attached_ = true;
  cold_attach_ = false;
  backoff_.RecordConnected(clock_->MonotonicMillis());
  last_ping_ms_ = clock_->MonotonicMillis();
  sent_columns_ = 0;
  sent_rows_ = 0;
  if (local_columns_ > 0 && local_rows_ > 0) {
    pending_columns_ = local_columns_;
    pending_rows_ = local_rows_;
    pending_resize_ms_ = clock_->MonotonicMillis();
  }
}

void Controller::PumpSession() {
  if (!mirror_) return;

  // The read deadline is not a poll interval: the transport also wakes on the command
  // notifier, so typed input is sent immediately. The deadline only has to be short
  // enough to publish a pending frame inside the coalescing window (spec §14).
  Millis timeout = 200;
  if (session_->NeedsPublish()) {
    Millis due = last_publish_ms_ + config_.snapshot_interval_ms - clock_->MonotonicMillis();
    timeout = std::min<Millis>(timeout, std::max<Millis>(1, due));
  }

  Status status = mirror_->Pump(timeout);

  // A callback may have asked to tear the session down. Doing it here, after Pump has
  // returned, keeps the MirrorSession alive for the whole of its own call stack.
  if (pending_disconnect_) {
    pending_disconnect_ = false;
    bool reconnect = pending_reconnect_;
    pending_reconnect_ = false;
    Disconnect(pending_disconnect_status_, reconnect);
    return;
  }

  if (status.ok()) return;
  if (status.kind() == ErrorKind::kTimeout) return;  // nothing to read, or interrupted
  if (status.kind() == ErrorKind::kCancelled) {
    Disconnect(status, false);
    return;
  }
  Disconnect(status, true);
}

void Controller::HandleMirrorEvent(const api::MirrorEvent& event, std::uint64_t generation) {
  // A callback from a superseded connection must never touch the current session
  // (spec §11).
  if (generation != generation_) return;

  switch (event.kind) {
    case api::MirrorEventKind::kReady:
      return;

    case api::MirrorEventKind::kSubscribed: {
      const api::SubscribedInfo& info = event.subscribed;
      if (info.columns > 0 && info.rows > 0) {
        // The publishing device owns the PTY size; the client renders what it is
        // told (relay reconciliation §1.5).
        session_->ResizeGrid(static_cast<int>(info.columns), static_cast<int>(info.rows));
      } else if (local_columns_ > 0 && local_rows_ > 0) {
        session_->ResizeGrid(local_columns_, local_rows_);
      }
      resume_offset_valid_ = true;
      resume_offset_ = mirror_->next_expected_offset();
      {
        std::lock_guard<std::mutex> lock(state_mutex_);
        status_.terminal_id = info.terminal_id;
        if (!info.label.empty()) status_.terminal_label = info.label;
        status_.input_available = mirror_->input_available();
        status_.columns = session_->emulator().columns();
        status_.rows = session_->emulator().rows();
      }
      SetState(ConnectionState::kAttached);
      if (!mirror_->input_available()) {
        Notify(ErrorKind::kInputRefused,
               info.accepts_input
                   ? "attached read-only: the publisher is not reachable for input"
                   : "attached read-only: this terminal does not accept input");
      }
      MaybePublishSnapshot(true);
      return;
    }

    case api::MirrorEventKind::kOutput:
      session_->ApplyOutput(event.payload);
      resume_offset_valid_ = true;
      resume_offset_ = mirror_->next_expected_offset();
      return;

    case api::MirrorEventKind::kGap:
      // The bytes before `available_from_offset` are gone, so the visible screen is
      // unrecoverable and must be rebuilt from this point (relay spec §6.2).
      session_->ResetTerminal();
      resume_offset_valid_ = true;
      resume_offset_ = event.available_from_offset;
      Notify(ErrorKind::kSyncFailure,
             "some output was dropped by the server; the screen was rebuilt");
      MaybePublishSnapshot(true);
      return;

    case api::MirrorEventKind::kDurable: {
      {
        std::lock_guard<std::mutex> lock(state_mutex_);
        status_.durable_offset = event.durable_offset;
      }
      MaybePersistResume(false);
      return;
    }

    case api::MirrorEventKind::kResize:
      session_->ResizeGrid(static_cast<int>(event.columns), static_cast<int>(event.rows));
      {
        std::lock_guard<std::mutex> lock(state_mutex_);
        status_.columns = session_->emulator().columns();
        status_.rows = session_->emulator().rows();
      }
      // The host needs to know the authoritative size changed, not only that a new
      // frame is available: the extra-key row and accessibility labels follow it.
      PublishStatus();
      MaybePublishSnapshot(true);
      return;

    case api::MirrorEventKind::kTerminalClosed:
      MaybePublishSnapshot(true);
      MaybePersistResume(true);
      Notify(ErrorKind::kTerminalClosed,
             "the remote terminal ended" +
                 (event.code.empty() ? std::string() : " (" + event.code + ")"));
      desired_terminal_id_.clear();
      SetState(ConnectionState::kTerminalClosed);
      pending_disconnect_ = true;
      pending_reconnect_ = false;
      pending_disconnect_status_ = Status::Error(ErrorKind::kTerminalClosed, "terminal closed");
      return;

    case api::MirrorEventKind::kInputAck: {
      {
        std::lock_guard<std::mutex> lock(state_mutex_);
        status_.unacknowledged_input_bytes = mirror_->unacknowledged_input_bytes();
      }
      PublishStatus();
      return;
    }

    case api::MirrorEventKind::kNotice:
      TM_LOG_INFO(kTag, "relay notice: %s", SanitizeForMessage(event.code, 40).c_str());
      return;

    case api::MirrorEventKind::kError: {
      if (StartsWith(event.code, "input_") || event.code == "rate_limited") {
        bool transient = api::IsTransientInputRefusal(event.code);
        if (!transient) {
          std::lock_guard<std::mutex> lock(state_mutex_);
          status_.input_available = false;
        }
        // Unacknowledged input is never resent (relay reconciliation §2.8): say so
        // rather than guessing.
        Notify(transient ? ErrorKind::kInputUndeliverable : ErrorKind::kInputRefused,
               transient ? "input was not delivered; try again"
                         : "this session may no longer send input");
        PublishStatus();
        return;
      }
      if (event.code == "offset_ahead") {
        // Normal after a relay restart, when the client saw bytes that were never
        // made durable: resubscribe for the whole retained window (relay spec §6.2).
        resume_offset_valid_ = false;
        resume_offset_ = 0;
        session_->ResetTerminal();
        Notify(ErrorKind::kSyncFailure, "resynchronising with the server");
        pending_disconnect_ = true;
        pending_reconnect_ = true;
        pending_disconnect_status_ =
            Status::Error(ErrorKind::kSyncFailure, "resubscribing from the retained window");
        return;
      }
      Notify(event.status.kind(), event.message.empty() ? event.status.message() : event.message);
      return;
    }
  }
}

void Controller::SendInputBytes(const std::string& bytes, bool from_user) {
  if (bytes.empty()) return;
  if (!attached_ || !mirror_ || !mirror_->subscribed()) {
    // spec §9.3: input produced while disconnected is rejected and surfaced, never
    // queued for silent replay.
    if (from_user) Notify(ErrorKind::kNetworkUnavailable, "not connected: that input was not sent");
    return;
  }
  if (!mirror_->input_available()) {
    if (from_user) Notify(ErrorKind::kInputRefused, "this terminal is attached read-only");
    return;
  }

  const std::size_t limit = static_cast<std::size_t>(mirror_->limits().max_input_frame_bytes);
  std::size_t position = 0;
  while (position < bytes.size()) {
    std::size_t length = std::min(limit, bytes.size() - position);
    Result<std::uint64_t> sent =
        mirror_->SendInput(ByteView::FromChars(bytes.data() + position, length));
    if (!sent.ok()) {
      if (from_user) Notify(sent.status().kind(), sent.status().message());
      return;
    }
    position += length;
  }
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    status_.unacknowledged_input_bytes = mirror_->unacknowledged_input_bytes();
  }
}

void Controller::MaybeSendResize() {
  // The publisher's size is the terminal's size (spec §10.4). The local geometry is
  // still tracked, because it is the fallback when the relay reports no size, but it
  // is never pushed at the far end.
  if (config_.follow_remote_size) return;
  if (pending_resize_ms_ < 0 || !attached_ || !mirror_) return;
  Millis now = clock_->MonotonicMillis();
  if (now < pending_resize_ms_) return;
  pending_resize_ms_ = -1;
  if (pending_columns_ == sent_columns_ && pending_rows_ == sent_rows_) return;
  if (!mirror_->input_available()) return;  // resize requests need input authority

  Status status = mirror_->RequestResize(static_cast<std::uint32_t>(pending_columns_),
                                         static_cast<std::uint32_t>(pending_rows_));
  if (status.ok()) {
    sent_columns_ = pending_columns_;
    sent_rows_ = pending_rows_;
  }
}

void Controller::MaybePublishSnapshot(bool force) {
  if (!session_) return;
  if (!force && !session_->NeedsPublish()) return;
  Millis now = clock_->MonotonicMillis();
  // Output may be coalesced between frames, but never dropped (spec §6.2, §10.1).
  if (!force && now - last_publish_ms_ < config_.snapshot_interval_ms) return;
  last_publish_ms_ = now;
  term::SnapshotRef snapshot = session_->PublishSnapshot();
  if (callbacks_.on_frame) callbacks_.on_frame(snapshot);
}

void Controller::MaybePersistResume(bool force) {
  if (preferences_ == nullptr || !mirror_ || desired_terminal_id_.empty()) return;
  Millis now = clock_->MonotonicMillis();
  if (!force && now - last_persist_ms_ < kPersistIntervalMs) return;
  last_persist_ms_ = now;
  std::uint64_t durable = mirror_->durable_offset();
  if (durable <= persisted_durable_) return;
  persisted_durable_ = durable;
  // Only a `durable` message may advance the persistent cursor: bytes above it are
  // live but not yet crash-durable (relay spec §6.2).
  preferences_->SetResumeOffset(desired_terminal_id_, durable, clock_->UnixMillis());
  Status saved = preferences_->Save();
  if (!saved.ok()) TM_LOG_WARN(kTag, "cannot persist the resume cursor");
}

void Controller::CheckHeartbeat() {
  if (!attached_ || !mirror_) return;
  Millis now = clock_->MonotonicMillis();
  Millis timeout = static_cast<Millis>(mirror_->limits().heartbeat_timeout_seconds) * 1000;
  if (timeout <= 0) timeout = config_.heartbeat_timeout_ms;
  if (now - mirror_->last_message_monotonic_ms() > timeout) {
    Disconnect(Status::Error(ErrorKind::kTimeout, "the connection stopped responding"), true);
    return;
  }
  Millis interval = static_cast<Millis>(mirror_->limits().heartbeat_interval_seconds) * 1000;
  if (interval <= 0) interval = kPingIntervalMs;
  if (now - last_ping_ms_ >= interval) {
    last_ping_ms_ = now;
    mirror_->SendPing();
  }
}

void Controller::Disconnect(const Status& reason, bool schedule_reconnect) {
  if (mirror_) {
    // Persist while the session still exists: after the reset there is no durable
    // offset left to record.
    MaybePersistResume(true);
    std::uint64_t unacknowledged = mirror_->unacknowledged_input_bytes();
    if (unacknowledged > 0) {
      // Input that was sent but never acknowledged must not be replayed on the next
      // connection (relay reconciliation §2.8).
      Notify(ErrorKind::kInputUndeliverable,
             "some typed input may not have reached the terminal");
    }
    mirror_->Close(net::ws_close::kNormal, "client detaching");
    mirror_.reset();
  }
  if (attached_) backoff_.RecordDisconnected(clock_->MonotonicMillis());
  attached_ = false;
  ++generation_;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    status_.input_available = false;
    status_.unacknowledged_input_bytes = 0;
  }

  if (schedule_reconnect && !desired_terminal_id_.empty()) {
    Millis delay = backoff_.NextDelay();
    next_attempt_ms_ = clock_->MonotonicMillis() + delay;
    SetState(ConnectionState::kReconnecting, reason);
    Notify(reason.kind(), reason.message());
  }
}

void Controller::SetState(ConnectionState state, const Status& error) {
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    status_.state = state;
    if (!error.ok()) status_.last_error = error;
    if (mirror_) {
      status_.next_offset = mirror_->next_expected_offset();
      status_.durable_offset = mirror_->durable_offset();
    }
  }
  PublishStatus();
}

void Controller::PublishStatus() {
  if (!callbacks_.on_status) return;
  SessionStatus copy;
  {
    std::lock_guard<std::mutex> lock(state_mutex_);
    copy = status_;
  }
  callbacks_.on_status(copy);
}

void Controller::Notify(ErrorKind kind, const std::string& message) {
  if (callbacks_.on_message) callbacks_.on_message(kind, message);
}

}  // namespace app
}  // namespace tmirror
