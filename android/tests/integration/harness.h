#pragma once

#include <memory>
#include <string>

#include "framework.h"
#include "tm/api/relay_client.h"
#include "tm/net/http_client.h"
#include "tm/util/json.h"

namespace tmtest {

/// Runs `tools/fake_relay/relay.py` as a child process for the duration of a test.
///
/// Integration tests must not need the production relay (spec §16.3), and they must be
/// able to reach its failure paths on demand — `gap`, `offset_ahead`, `slow_consumer`,
/// `terminal.closed`, token expiry, every input refusal — which is exactly what the
/// fake exposes under `/_test/`.
class FakeRelay {
 public:
  FakeRelay();
  ~FakeRelay();

  FakeRelay(const FakeRelay&) = delete;
  FakeRelay& operator=(const FakeRelay&) = delete;

  /// Starts the relay. Returns false when Python is unavailable, in which case the
  /// caller skips rather than fails.
  bool Start(const std::vector<std::string>& extra_arguments = {});
  void Stop();

  bool running() const { return pid_ > 0; }
  std::uint16_t port() const { return port_; }
  std::string base_url() const;

  /// Calls a `/_test/` endpoint. Returns the parsed response.
  tmirror::Result<tmirror::Json> Control(const std::string& path,
                                         const tmirror::Json& body,
                                         const std::string& method = "POST");

  /// Convenience wrappers for the actions tests use most.
  std::string CreateTerminal(const std::string& label = "shell", int columns = 80,
                             int rows = 24, bool accepts_input = true);
  bool Emit(const std::string& terminal_id, const std::string& text, int repeat = 1);
  bool EmitBytes(const std::string& terminal_id, const std::string& raw_bytes);
  bool SetDurable(const std::string& terminal_id, std::uint64_t offset);
  bool Resize(const std::string& terminal_id, int columns, int rows);
  bool Close(const std::string& terminal_id, const std::string& reason = "process_exited");
  bool Evict(const std::string& terminal_id, std::uint64_t bytes);
  bool Drop(const std::string& terminal_id, int code = 1001);
  bool SetPolicy(const std::string& key, const tmirror::Json& value);
  bool SetInputAvailable(const std::string& terminal_id, bool available);
  /// Everything the fake received as terminal input, for assertions.
  std::string ReceivedInput(const std::string& terminal_id);
  int ResizeRequestCount(const std::string& terminal_id);

  /// Registers a fresh identity and a `client`-role device, mirroring the pairing
  /// flow in the relay reconciliation §2.2: the identity authorises, the device signs.
  struct PairedDevice {
    std::string identity_id;
    std::string device_id;
    tmirror::Bytes device_seed;
  };
  tmirror::Result<PairedDevice> PairClientDevice();

  /// The owner's half of pairing on its own: an identity and a token that may
  /// register devices under it. This is what a pairing code carries, and what lets a
  /// device enrol itself by signing its own challenge (relay spec §5.2).
  struct Owner {
    std::string identity_id;
    std::string identity_token;
  };
  tmirror::Result<Owner> CreateOwnerIdentity();

 private:
  int pid_ = -1;
  std::uint16_t port_ = 0;
};

/// Polls `predicate` until it holds or the deadline passes. Integration tests are
/// inherently asynchronous; sleeping a fixed amount would be both slower and flakier.
bool WaitFor(const std::function<bool()>& predicate, int timeout_ms = 5000);

/// Path to the repository root, injected by CMake.
std::string RepoRoot();

}  // namespace tmtest
