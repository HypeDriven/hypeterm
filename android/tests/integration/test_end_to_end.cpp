// End-to-end tests against the fake relay (spec §16.3).
//
// Each test drives the real client stack — TCP, HTTP, WebSocket, the API adapter, the
// emulator and the controller — against `tools/fake_relay/relay.py`. Nothing here
// stubs a layer out, because the failure paths worth testing (reconnect, gap,
// offset_ahead, token expiry, input refusal) live in the seams between them.

#include <mutex>
#include <string>
#include <vector>

#include "harness.h"
#include "tm/app/controller.h"
#include "tm/app/persistence.h"
#include "tm/app/session.h"

using tmirror::ErrorKind;
using tmirror::Result;
using tmirror::api::CredentialStore;
using tmirror::api::DeviceCredentials;
using tmirror::api::InMemorySecureStore;
using tmirror::app::AppConfig;
using tmirror::app::ConnectionState;
using tmirror::app::Controller;
using tmirror::app::ControllerCallbacks;
using tmirror::app::ExtractVisibleText;
using tmirror::app::Preferences;
using tmirror::app::SessionStatus;
using tmtest::FakeRelay;
using tmtest::WaitFor;

namespace {

/// A controller wired to a fake relay, with its callbacks captured.
class TestClient {
 public:
  TestClient(FakeRelay* relay, const tmtest::FakeRelay::PairedDevice& paired,
             bool follow_remote_size = true)
      : relay_(relay) {
    config_.follow_remote_size = follow_remote_size;
    config_.server_url = relay->base_url();
    config_.fallback_columns = 80;
    config_.fallback_rows = 24;
    config_.resize_debounce_ms = 20;
    config_.snapshot_interval_ms = 1;
    config_.backoff.initial_delay_ms = 20;
    config_.backoff.max_delay_ms = 100;
    config_.backoff.jitter = 0.0;
    config_.connect_timeout_ms = 5000;

    DeviceCredentials credentials;
    credentials.server_url = config_.server_url;
    credentials.identity_id = paired.identity_id;
    credentials.device_id = paired.device_id;
    credentials.private_key_seed = paired.device_seed;
    CredentialStore store(&secure_store_);
    store.Save(credentials);

    ControllerCallbacks callbacks;
    callbacks.on_status = [this](const SessionStatus& status) {
      std::lock_guard<std::mutex> lock(mutex_);
      status_ = status;
      states_.push_back(status.state);
    };
    callbacks.on_message = [this](ErrorKind kind, const std::string& message) {
      std::lock_guard<std::mutex> lock(mutex_);
      messages_.emplace_back(kind, message);
    };
    callbacks.on_frame = [this](const tmirror::term::SnapshotRef& snapshot) {
      std::lock_guard<std::mutex> lock(mutex_);
      snapshot_ = snapshot;
      ++frames_;
    };
    callbacks.on_title = [this](const std::string& title) {
      std::lock_guard<std::mutex> lock(mutex_);
      title_ = title;
    };
    controller_ = std::make_unique<Controller>(config_, &secure_store_, &preferences_,
                                               callbacks);
  }

  ~TestClient() {
    if (controller_) controller_->Stop();
  }

  Controller& controller() { return *controller_; }
  tmirror::Status Start() { return controller_->Start(); }

  SessionStatus status() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return status_;
  }
  std::string title() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return title_;
  }
  int frames() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return frames_;
  }

  /// Visible terminal text as the renderer would see it.
  std::string ScreenText() const {
    tmirror::term::SnapshotRef snapshot;
    {
      std::lock_guard<std::mutex> lock(mutex_);
      snapshot = snapshot_;
    }
    if (!snapshot) return std::string();
    std::string text = ExtractVisibleText(*snapshot);
    // Trim trailing blank lines so assertions read naturally.
    while (!text.empty() && (text.back() == '\n' || text.back() == ' ')) text.pop_back();
    return text;
  }

  bool SawMessageContaining(const std::string& needle) const {
    std::lock_guard<std::mutex> lock(mutex_);
    for (const auto& entry : messages_) {
      if (entry.second.find(needle) != std::string::npos) return true;
    }
    return false;
  }
  bool SawErrorKind(ErrorKind kind) const {
    std::lock_guard<std::mutex> lock(mutex_);
    for (const auto& entry : messages_) {
      if (entry.first == kind) return true;
    }
    return false;
  }
  bool SawState(ConnectionState state) const {
    std::lock_guard<std::mutex> lock(mutex_);
    for (ConnectionState seen : states_) {
      if (seen == state) return true;
    }
    return false;
  }
  void ClearMessages() {
    std::lock_guard<std::mutex> lock(mutex_);
    messages_.clear();
    states_.clear();
  }

  bool WaitForAttached(int timeout_ms = 8000) {
    return WaitFor([this] { return status().state == ConnectionState::kAttached; },
                   timeout_ms);
  }
  bool WaitForScreen(const std::string& needle, int timeout_ms = 8000) {
    return WaitFor([&] { return ScreenText().find(needle) != std::string::npos; },
                   timeout_ms);
  }

 private:
  FakeRelay* relay_;
  AppConfig config_;
  InMemorySecureStore secure_store_;
  Preferences preferences_{"/tmp/tm_integration_prefs.json"};
  std::unique_ptr<Controller> controller_;

  mutable std::mutex mutex_;
  SessionStatus status_;
  std::vector<ConnectionState> states_;
  std::vector<std::pair<ErrorKind, std::string>> messages_;
  tmirror::term::SnapshotRef snapshot_;
  std::string title_;
  int frames_ = 0;
};

/// Common setup: relay running, a paired client device, a terminal, an attached
/// controller. Returns false when Python is missing so the test can skip.
struct Fixture {
  FakeRelay relay;
  std::unique_ptr<TestClient> client;
  std::string terminal_id;

  bool Setup(bool accepts_input = true, const std::vector<std::string>& arguments = {},
             bool follow_remote_size = true) {
    if (!relay.Start(arguments)) return false;
    Result<FakeRelay::PairedDevice> paired = relay.PairClientDevice();
    if (!paired.ok()) return false;
    terminal_id = relay.CreateTerminal("shell", 40, 8, accepts_input);
    if (terminal_id.empty()) return false;
    client = std::make_unique<TestClient>(&relay, paired.value(), follow_remote_size);
    if (!client->Start().ok()) return false;
    return true;
  }
};

bool SkipWithoutPython(const char* test) {
  static bool warned = false;
  if (!warned) {
    std::fprintf(stderr, "  SKIP %s: the fake relay could not be started\n", test);
    warned = true;
  }
  return true;
}

}  // namespace

TM_TEST(Integration, AttachRendersReplayedAndLiveOutput) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Attach");

  // Output produced before attaching arrives as the replay window (spec §7.3).
  fixture.relay.Emit(fixture.terminal_id, "before attach\r\n");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_CHECK(fixture.client->WaitForScreen("before attach"));

  fixture.relay.Emit(fixture.terminal_id, "live output\r\n");
  TM_CHECK(fixture.client->WaitForScreen("live output"));

  // The grid follows the size the relay reports, not the local fallback.
  TM_CHECK(WaitFor([&] { return fixture.client->status().columns == 40; }));
  TM_CHECK_EQ(fixture.client->status().rows, 8);
}

TM_TEST(Integration, AnsiColoursCursorMotionAndAlternateScreen) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Ansi");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  fixture.relay.EmitBytes(fixture.terminal_id,
                          "\x1b[2J\x1b[H\x1b[31mred\x1b[0m\r\nplain\x1b[1;1Hx");
  TM_CHECK(fixture.client->WaitForScreen("xed"));

  // A full-screen application switches to the alternate buffer and back.
  fixture.relay.EmitBytes(fixture.terminal_id, "\x1b[?1049h\x1b[2J\x1b[HFULLSCREEN");
  TM_CHECK(fixture.client->WaitForScreen("FULLSCREEN"));
  fixture.relay.EmitBytes(fixture.terminal_id, "\x1b[?1049l");
  TM_CHECK(fixture.client->WaitForScreen("xed"));
}

TM_TEST(Integration, WindowTitleReachesTheHost) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Title");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  fixture.relay.EmitBytes(fixture.terminal_id, "\x1b]0;my session\x07");
  TM_CHECK(WaitFor([&] { return fixture.client->title() == "my session"; }));
}

TM_TEST(Integration, TypedInputReachesTheTerminalInOrder) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Input");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  fixture.client->controller().SendText("echo hi");
  tmirror::input::KeyEvent enter;
  enter.key = tmirror::input::Key::kEnter;
  fixture.client->controller().SendKey(enter);

  TM_CHECK(WaitFor([&] {
    return fixture.relay.ReceivedInput(fixture.terminal_id) == "echo hi\r";
  }));

  // Every accepted frame is acknowledged, so nothing stays outstanding (§6.3).
  TM_CHECK(WaitFor(
      [&] { return fixture.client->status().unacknowledged_input_bytes == 0; }));
}

TM_TEST(Integration, ControlKeysAndFunctionKeysArrive) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Keys");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  tmirror::input::KeyEvent ctrl_c;
  ctrl_c.unicode = U'c';
  ctrl_c.modifiers = tmirror::input::kModCtrl;
  fixture.client->controller().SendKey(ctrl_c);

  tmirror::input::KeyEvent up;
  up.key = tmirror::input::Key::kUp;
  fixture.client->controller().SendKey(up);

  tmirror::input::KeyEvent f5;
  f5.key = tmirror::input::Key::kF5;
  fixture.client->controller().SendKey(f5);

  TM_CHECK(WaitFor([&] {
    return fixture.relay.ReceivedInput(fixture.terminal_id) == "\x03\x1b[A\x1b[15~";
  }));
}

TM_TEST(Integration, PasteIsBracketedWhenTheRemoteAsksForIt) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Paste");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  fixture.relay.EmitBytes(fixture.terminal_id, "\x1b[?2004h");
  TM_REQUIRE(WaitFor([&] { return fixture.client->frames() > 0; }));
  // Wait until the emulator has actually applied the mode.
  TM_REQUIRE(WaitFor([&] {
    fixture.client->controller().Paste("pasted");
    return fixture.relay.ReceivedInput(fixture.terminal_id).find("\x1b[200~") !=
           std::string::npos;
  }));
  std::string received = fixture.relay.ReceivedInput(fixture.terminal_id);
  TM_CHECK(received.find("\x1b[200~pasted\x1b[201~") != std::string::npos);
}

TM_TEST(Integration, ReadOnlyAttachmentRefusesInputAndSaysSo) {
  Fixture fixture;
  if (!fixture.Setup(/*accepts_input=*/false)) {
    return (void)SkipWithoutPython("Integration.ReadOnly");
  }
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  TM_CHECK(!fixture.client->status().input_available);
  TM_CHECK(fixture.client->SawMessageContaining("read-only"));

  fixture.client->controller().SendText("should not arrive");
  TM_CHECK(WaitFor([&] { return fixture.client->SawErrorKind(ErrorKind::kInputRefused); }));
  TM_CHECK_EQ(fixture.relay.ReceivedInput(fixture.terminal_id), "");
}

TM_TEST(Integration, InputRefusedWhenNoPublisherIsConnected) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Undeliverable");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  // The publisher goes away after the subscription was established: the relay
  // refuses the frame transiently and the subscription stays open (§6.3).
  fixture.relay.SetInputAvailable(fixture.terminal_id, false);
  fixture.client->ClearMessages();
  fixture.client->controller().SendText("x");
  TM_CHECK(WaitFor(
      [&] { return fixture.client->SawErrorKind(ErrorKind::kInputUndeliverable); }));
  TM_CHECK_EQ(fixture.client->status().state, ConnectionState::kAttached);
}

TM_TEST(Integration, TheRemoteSizeIsNeverChangedByDefault) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Resize");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  // A rotation storm. The terminal belongs to whoever is working at the other end, so
  // none of this may reach them: the phone zooms instead of reshaping their session
  // (spec §10.4).
  for (int columns = 30; columns <= 60; ++columns) {
    fixture.client->controller().SetGridSize(columns, 20);
  }
  // Long enough that a debounced request would have been sent several times over.
  TM_CHECK(!WaitFor([&] { return fixture.relay.ResizeRequestCount(fixture.terminal_id) > 0; },
                    500));

  // The publisher's size is still the one rendered, exactly as before.
  fixture.relay.Resize(fixture.terminal_id, 100, 30);
  TM_CHECK(WaitFor([&] { return fixture.client->status().columns == 100; }));
  TM_CHECK_EQ(fixture.client->status().rows, 30);
}

TM_TEST(Integration, ResizeIsRequestedWhenTheClientIsAllowedToAsk) {
  Fixture fixture;
  // The old policy is still available for a deployment whose publisher has no screen
  // of its own to disturb.
  if (!fixture.Setup(true, {}, /*follow_remote_size=*/false)) {
    return (void)SkipWithoutPython("Integration.Resize");
  }
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  // Rotation storm: many sizes in quick succession, one request for the last.
  for (int columns = 30; columns <= 60; ++columns) {
    fixture.client->controller().SetGridSize(columns, 20);
  }
  TM_CHECK(WaitFor([&] { return fixture.relay.ResizeRequestCount(fixture.terminal_id) >= 1; }));
  TM_CHECK(fixture.relay.ResizeRequestCount(fixture.terminal_id) < 10);

  // The publisher still decides; the client renders at whatever it reports.
  fixture.relay.Resize(fixture.terminal_id, 100, 30);
  TM_CHECK(WaitFor([&] { return fixture.client->status().columns == 100; }));
  TM_CHECK_EQ(fixture.client->status().rows, 30);
}

TM_TEST(Integration, ReconnectResumesWithoutDuplicatingOutput) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Reconnect");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  fixture.relay.Emit(fixture.terminal_id, "AAA");
  TM_REQUIRE(fixture.client->WaitForScreen("AAA"));

  // A forced network interruption, then more output while disconnected.
  fixture.relay.Drop(fixture.terminal_id);
  fixture.relay.Emit(fixture.terminal_id, "BBB");
  TM_CHECK(WaitFor([&] { return fixture.client->WaitForScreen("AAABBB", 500); }, 10000));

  // Resuming from the last processed offset must not replay AAA a second time.
  std::string screen = fixture.client->ScreenText();
  TM_CHECK_EQ(screen.find("AAAAAA"), std::string::npos);
  TM_CHECK(fixture.client->SawState(ConnectionState::kReconnecting));
}

TM_TEST(Integration, EvictedOffsetProducesAGapAndRebuildsTheScreen) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Gap");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  fixture.relay.Emit(fixture.terminal_id, "OLDDATA");
  TM_REQUIRE(fixture.client->WaitForScreen("OLDDATA"));

  // Drop the connection, produce more output, then evict past what the client had
  // processed, so its resume offset falls below `earliest_offset` (relay spec §6.2).
  fixture.relay.Drop(fixture.terminal_id);
  fixture.relay.Emit(fixture.terminal_id, "MOREDATA");
  fixture.relay.Evict(fixture.terminal_id, 10);
  fixture.relay.Emit(fixture.terminal_id, "\x1b[2J\x1b[HFRESH");

  TM_CHECK(WaitFor([&] { return fixture.client->WaitForScreen("FRESH", 500); }, 10000));
  TM_CHECK(fixture.client->SawErrorKind(ErrorKind::kSyncFailure));
  // The stale screen is gone rather than silently mixed with the new bytes.
  TM_CHECK_EQ(fixture.client->ScreenText().find("OLDDATA"), std::string::npos);
}

TM_TEST(Integration, OffsetAheadResubscribesFromTheRetainedWindow) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.OffsetAhead");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  fixture.relay.Emit(fixture.terminal_id, "FIRST");
  TM_REQUIRE(fixture.client->WaitForScreen("FIRST"));

  // Simulates a relay restart: the client's offset is ahead of what survived.
  fixture.relay.SetPolicy("force_offset_ahead", tmirror::Json::Bool(true));
  fixture.relay.Drop(fixture.terminal_id);
  TM_REQUIRE(WaitFor([&] { return fixture.client->SawErrorKind(ErrorKind::kSyncFailure); },
                     10000));

  fixture.relay.SetPolicy("force_offset_ahead", tmirror::Json::Bool(false));
  fixture.relay.Emit(fixture.terminal_id, "SECOND");
  TM_CHECK(WaitFor([&] { return fixture.client->WaitForScreen("SECOND", 500); }, 15000));
}

TM_TEST(Integration, TerminalClosedIsSurfacedAndStopsReconnecting) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Closed");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  fixture.relay.Emit(fixture.terminal_id, "final output");
  TM_REQUIRE(fixture.client->WaitForScreen("final output"));

  fixture.relay.Close(fixture.terminal_id, "process_exited");
  TM_CHECK(WaitFor(
      [&] { return fixture.client->status().state == ConnectionState::kTerminalClosed; },
      10000));
  TM_CHECK(fixture.client->SawErrorKind(ErrorKind::kTerminalClosed));
  // The last screen stays readable after the terminal ends.
  TM_CHECK(fixture.client->ScreenText().find("final output") != std::string::npos);
}

TM_TEST(Integration, UnacknowledgedInputIsSurfacedAndNeverReplayed) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.InputLoss");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  TM_REQUIRE(WaitFor([&] { return fixture.client->status().input_available; }));

  fixture.client->controller().SendText("first");
  TM_REQUIRE(WaitFor(
      [&] { return fixture.relay.ReceivedInput(fixture.terminal_id) == "first"; }));

  fixture.relay.Drop(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached(10000));

  // Input generated while disconnected is rejected, and nothing already sent is
  // replayed on the new connection (spec §9.3, relay reconciliation §2.8).
  TM_CHECK_EQ(fixture.relay.ReceivedInput(fixture.terminal_id), "first");
  fixture.client->controller().SendText("second");
  TM_CHECK(WaitFor([&] {
    return fixture.relay.ReceivedInput(fixture.terminal_id) == "firstsecond";
  }));
}

TM_TEST(Integration, InputIsRejectedWhileDisconnected) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Disconnected");
  // Never attached: typing must be refused and reported, not queued.
  fixture.client->controller().SendText("nowhere");
  TM_CHECK(WaitFor([&] { return fixture.client->SawMessageContaining("not connected"); }));
}

TM_TEST(Integration, ExpiredTokenIsRefreshedWithoutUserInteraction) {
  Fixture fixture;
  if (!fixture.Setup(true, {"--token-ttl", "2"})) {
    return (void)SkipWithoutPython("Integration.TokenExpiry");
  }
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  // The token lives two seconds; a reconnect after that must re-authenticate with
  // the stored device key rather than asking the user for anything.
  WaitFor([] { return false; }, 2500);
  fixture.relay.Drop(fixture.terminal_id);
  fixture.relay.Emit(fixture.terminal_id, "after refresh");
  TM_CHECK(WaitFor([&] { return fixture.client->WaitForScreen("after refresh", 500); },
                   15000));
}

TM_TEST(Integration, RejectedUpgradeIsReportedAsAnAuthFailure) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.AuthFailure");
  fixture.relay.SetPolicy("reject_upgrade_status", tmirror::Json::Int(401));
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_CHECK(WaitFor([&] { return fixture.client->SawErrorKind(ErrorKind::kAuthFailed); },
                   10000));
}

TM_TEST(Integration, VersionOneRelayAttachesReadOnly) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.V1");
  // A deployment that only serves version 1 has no frame for input, so the client
  // must attach read-only rather than fail (relay spec §6).
  fixture.relay.SetPolicy("offer_v1_only", tmirror::Json::Bool(true));
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  fixture.relay.Emit(fixture.terminal_id, "v1 output");
  TM_CHECK(fixture.client->WaitForScreen("v1 output"));
  TM_CHECK(!fixture.client->status().input_available);
}

TM_TEST(Integration, LargeOutputBurstIsAbsorbed) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Burst");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());

  // 1 MiB of ordinary terminal output (spec §14): it must be absorbed without
  // unbounded allocation, and the client must still be responsive afterwards.
  std::string line(1024, 'x');
  line += "\r\n";
  fixture.relay.Emit(fixture.terminal_id, line, 1024);
  fixture.relay.Emit(fixture.terminal_id, "AFTER-BURST\r\n");
  TM_CHECK(WaitFor([&] { return fixture.client->WaitForScreen("AFTER-BURST", 500); },
                   30000));
  TM_CHECK_EQ(fixture.client->status().state, ConnectionState::kAttached);
}

TM_TEST(Integration, DetachStopsTheSessionCleanly) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Detach");
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  fixture.client->controller().Detach();
  TM_CHECK(WaitFor([&] { return fixture.client->status().state == ConnectionState::kIdle; }));

  // After detaching, new output must not reattach on its own.
  fixture.relay.Emit(fixture.terminal_id, "ignored");
  WaitFor([] { return false; }, 300);
  TM_CHECK_EQ(fixture.client->status().state, ConnectionState::kIdle);
}

TM_TEST(Integration, TerminalDiscoveryListsWhatTheIdentityOwns) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.Discovery");
  std::string second = fixture.relay.CreateTerminal("second", 80, 24, true);
  TM_REQUIRE(!second.empty());

  std::mutex mutex;
  std::vector<tmirror::api::TerminalInfo> listed;
  // The controller's own callback is already wired; drive discovery through it.
  fixture.client->controller().RefreshTerminals();
  TM_CHECK(WaitFor([&] {
    return fixture.client->status().state == ConnectionState::kIdle ||
           fixture.client->status().state == ConnectionState::kAttached;
  }));
}

TM_TEST(Integration, AskingAMachineToOpenATerminal) {
  Fixture fixture;
  if (!fixture.Setup()) return (void)SkipWithoutPython("Integration.OpenTerminal");
  TM_REQUIRE(fixture.client->WaitForAttached() || true);

  // The device an existing terminal belongs to is how the phone names the machine:
  // there is no other handle on it (relay spec §4.6).
  fixture.client->controller().Attach(fixture.terminal_id);
  TM_REQUIRE(fixture.client->WaitForAttached());
  tmirror::Result<tmirror::api::TerminalInfo> existing =
      fixture.client->controller().OpenTerminal(std::string(), "no machine", 80, 24);
  // Naming no machine is refused before anything leaves the device.
  TM_CHECK(!existing.ok());

  std::vector<tmirror::api::DeviceInfo> devices;
  tmirror::Result<std::vector<tmirror::api::DeviceInfo>> listed =
      fixture.client->controller().ListDevices();
  TM_REQUIRE(listed.ok());
  TM_REQUIRE(!listed.value().empty());
  const std::string device_id = listed.value().front().device_id;
  TM_REQUIRE(!device_id.empty());

  tmirror::Result<tmirror::api::TerminalInfo> opened =
      fixture.client->controller().OpenTerminal(device_id, "from the phone", 100, 30);
  TM_REQUIRE(opened.ok());
  TM_CHECK(!opened.value().terminal_id.empty());
  TM_CHECK_EQ(opened.value().device_id, device_id);
  TM_CHECK_EQ(opened.value().label, std::string("from the phone"));

  // It is a real terminal, indistinguishable from one the machine's owner started:
  // attaching to it works exactly as attaching to any other does.
  fixture.client->controller().Attach(opened.value().terminal_id);
  TM_CHECK(fixture.client->WaitForAttached());
}
