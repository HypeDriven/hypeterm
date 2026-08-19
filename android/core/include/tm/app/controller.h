#pragma once

#include <atomic>
#include <condition_variable>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include "tm/api/credentials.h"
#include "tm/api/mirror_session.h"
#include "tm/api/relay_client.h"
#include "tm/app/config.h"
#include "tm/app/persistence.h"
#include "tm/app/session.h"
#include "tm/input/key_encoder.h"
#include "tm/util/backoff.h"
#include "tm/util/queue.h"

namespace tmirror {
namespace app {

/// User-visible connection state (spec §5.1, §15).
enum class ConnectionState {
  kIdle,
  kPairingRequired,
  kAuthenticating,
  kDiscovering,
  kAttaching,
  kAttached,
  kReconnecting,
  kTerminalClosed,
  kFailed,
  kStopped,
};

const char* ConnectionStateName(ConnectionState state);

struct SessionStatus {
  ConnectionState state = ConnectionState::kIdle;
  bool input_available = false;
  bool network_available = true;
  std::string terminal_id;
  std::string terminal_label;
  int columns = 0;
  int rows = 0;
  std::uint64_t next_offset = 0;
  std::uint64_t durable_offset = 0;
  std::uint64_t unacknowledged_input_bytes = 0;
  Status last_error;
};

/// Callbacks are invoked on the network thread. The Android host layer marshals them
/// to the UI or render thread; nothing here touches a platform API directly.
struct ControllerCallbacks {
  std::function<void(const SessionStatus&)> on_status;
  std::function<void(const term::SnapshotRef&)> on_frame;
  std::function<void(const std::vector<api::TerminalInfo>&)> on_terminals;
  std::function<void(const std::string& title)> on_title;
  /// Errors and notices meant for the user, already classified (spec §15).
  std::function<void(ErrorKind kind, const std::string& message)> on_message;
  std::function<void()> on_bell;
  /// OSC 52 write, only when the policy allows it (spec §8.1).
  std::function<void(const std::string& text)> on_clipboard_write;
};

/// What the pairing screen needs to display (relay reconciliation §2.2).
struct PairingInfo {
  std::string public_key_base64url;
  std::string key_fingerprint;
  std::string server_url;
  std::string device_name;
};

/// The native application controller (spec §6.1).
///
/// Owns the session lifecycle, the reconnect policy, the generation counter that keeps
/// stale callbacks from touching a newer session (spec §11), and the routing between
/// the UI, the relay and the emulator. Every public method is safe to call from the UI
/// thread and none of them block on I/O.
class Controller {
 public:
  Controller(AppConfig config, api::SecureStore* secure_store, Preferences* preferences,
             ControllerCallbacks callbacks, Clock* clock = Clock::System());
  ~Controller();

  Controller(const Controller&) = delete;
  Controller& operator=(const Controller&) = delete;

  Status Start();
  void Stop();

  /// Sets the relay URL before the controller starts.
  ///
  /// Pairing happens *before* there is a relay to connect to, so the URL the user
  /// types has to be able to reach a controller that was constructed without one.
  /// Refused once the controller is running, because the relay client and any live
  /// session are bound to the old origin; stop and start again to change it.
  Status SetServerUrl(const std::string& url);
  std::string server_url() const;

  // ------------------------------------------------------------------ pairing
  /// Generates a fresh device key and returns what the pairing screen must show.
  /// Nothing is persisted until `CompletePairing` succeeds.
  Result<PairingInfo> BeginPairing();
  /// Records the owner identity and the device ID the owner's machine registered.
  ///
  /// Only usable against a relay that lets the owner vouch for a key on its own. The
  /// real relay does not — it requires the device to sign its own registration — so
  /// `CompletePairingWithCode` is the flow that works in production.
  Status CompletePairing(const std::string& identity_id, const std::string& device_id);

  /// Finishes pairing from a code the owner's machine produced (relay spec §5.2).
  ///
  /// The code carries the relay URL, the owning identity and a short-lived identity
  /// token. This device signs its *own* registration challenge with the key generated
  /// by `BeginPairing`, so its private half never leaves it, and the borrowed token
  /// only authorises the request. Both halves of the exchange are still present; they
  /// just travel in one string instead of a conversation.
  ///
  /// Blocking: it performs several HTTP requests. Call it off the UI thread.
  /// Returns the relay URL the code carried, which is where the client must now
  /// point: pairing is often the moment that address is learned.
  Result<std::string> CompletePairingWithCode(const std::string& code);
  /// Asks a machine to open a terminal, and returns the one it opened
  /// (relay spec §4.6).
  ///
  /// Blocking: it waits on a round trip to the far machine, which has to start a
  /// process. Deliberately not a queued command — the network thread runs the mirror's
  /// read loop, and holding it for that long risks the relay closing the subscription
  /// as a slow consumer. Call it off the UI thread and off the network thread.
  Result<api::TerminalInfo> OpenTerminal(const std::string& device_id, const std::string& label,
                                         int columns, int rows);

  /// The devices this identity owns, so the user can choose which machine to ask.
  /// Blocking, for the same reason.
  Result<std::vector<api::DeviceInfo>> ListDevices();

  /// Discards the stored credential (sign-out). The identity is unaffected; ending
  /// access from the server side means revoking the device.
  Status ForgetCredentials();
  /// Not const: it lazily loads the stored credential, so a caller can ask before
  /// Start() has run.
  bool HasCredentials();

  // ----------------------------------------------------------------- commands
  bool RefreshTerminals();
  bool Attach(const std::string& terminal_id);
  bool Detach();
  bool SendKey(const input::KeyEvent& event);
  bool SendText(const std::string& utf8_text);
  bool Paste(const std::string& utf8_text);
  bool SendMouse(const input::MouseEvent& event);
  bool SetFocused(bool focused);
  /// Reports the grid the renderer computed. Debounced, and the final size is always
  /// sent (spec §10.3); the publisher decides whether to honour it.
  bool SetGridSize(int columns, int rows);
  bool ScrollLines(int delta);
  bool ScrollToBottom();
  bool SetSelection(const term::Selection& selection);
  bool ClearSelection();
  bool SetPaused(bool paused);
  bool SetNetworkAvailable(bool available);

  // ------------------------------------------------------------------ reading
  term::SnapshotRef LatestSnapshot() const;
  SessionStatus status() const;
  /// Text of the current selection, computed from the latest snapshot.
  std::string SelectedText() const;
  std::string VisibleText() const;

 private:
  struct Command {
    enum class Type {
      kRefreshTerminals,
      kAttach,
      kDetach,
      kKey,
      kText,
      kPaste,
      kMouse,
      kFocus,
      kResize,
      kScroll,
      kScrollToBottom,
      kSelection,
      kClearSelection,
      kPause,
      kNetwork,
      kShutdown,
    };
    Type type = Type::kRefreshTerminals;
    std::string text;
    input::KeyEvent key;
    input::MouseEvent mouse;
    term::Selection selection;
    int a = 0;
    int b = 0;
    bool flag = false;
  };

  bool Post(Command command, std::size_t weight = 0);
  void NetworkThreadMain();
  void ProcessCommands();
  void HandleCommand(Command& command);
  void MaybeConnect();
  void PumpSession();
  void HandleMirrorEvent(const api::MirrorEvent& event, std::uint64_t generation);
  void MaybePublishSnapshot(bool force);
  void MaybeSendResize();
  void MaybePersistResume(bool force);
  void CheckHeartbeat();
  void Disconnect(const Status& reason, bool schedule_reconnect);
  void SetState(ConnectionState state, const Status& error = Status::Ok());
  void Notify(ErrorKind kind, const std::string& message);
  Status EnsureToken();
  /// Queues bytes for the remote terminal, applying the relay's frame-size limit and
  /// the disconnected-input policy (spec §9.3).
  void SendInputBytes(const std::string& bytes, bool from_user);
  void PublishStatus();
  /// Loads the stored credential at most once. Caller holds `state_mutex_`.
  void EnsureCredentialsLoaded();

  AppConfig config_;
  api::SecureStore* secure_store_;
  Preferences* preferences_;
  ControllerCallbacks callbacks_;
  Clock* clock_;

  api::CredentialStore credential_store_;
  std::unique_ptr<api::DeviceCredentials> credentials_;
  bool credentials_load_attempted_ = false;
  std::unique_ptr<api::RelayClient> relay_;
  std::unique_ptr<api::MirrorSession> mirror_;
  std::unique_ptr<TerminalSession> session_;
  api::AccessToken token_;
  Backoff backoff_;
  net::Notifier notifier_;
  std::shared_ptr<net::CancelSignal> cancel_;

  BoundedQueue<Command> commands_;
  std::thread thread_;
  std::atomic<bool> running_{false};

  mutable std::mutex state_mutex_;
  SessionStatus status_;

  // Network-thread state.
  std::uint64_t generation_ = 0;
  std::string desired_terminal_id_;
  bool attached_ = false;
  bool cold_attach_ = true;
  bool resume_offset_valid_ = false;
  std::uint64_t resume_offset_ = 0;
  Millis next_attempt_ms_ = 0;
  Millis last_publish_ms_ = 0;
  Millis last_persist_ms_ = 0;
  Millis last_ping_ms_ = 0;
  Millis pending_resize_ms_ = -1;
  Millis paused_since_ms_ = -1;
  int pending_columns_ = 0;
  int pending_rows_ = 0;
  int sent_columns_ = 0;
  int sent_rows_ = 0;
  int local_columns_ = 0;
  int local_rows_ = 0;
  bool paused_ = false;
  /// Set from inside a mirror callback; acted on after Pump returns, because
  /// destroying the session from within its own read loop would be a use-after-free.
  bool pending_disconnect_ = false;
  bool pending_reconnect_ = false;
  Status pending_disconnect_status_;
  bool network_available_ = true;
  bool focused_ = true;
  input::DuplicateTextFilter duplicate_filter_;
  std::uint64_t persisted_durable_ = 0;
};

}  // namespace app
}  // namespace tmirror
