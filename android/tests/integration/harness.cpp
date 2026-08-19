#include "harness.h"

#include <signal.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <cstring>
#include <thread>
#include <vector>

#include "tm/api/credentials.h"
#include "tm/crypto/crypto.h"
#include "tm/util/base64.h"
#include "tm/util/strings.h"

namespace tmtest {
namespace {

using tmirror::ByteView;
using tmirror::Json;
using tmirror::Result;
using tmirror::Status;

}  // namespace

std::string RepoRoot() {
#if defined(TM_REPO_ROOT)
  return TM_REPO_ROOT;
#else
  return ".";
#endif
}

FakeRelay::FakeRelay() = default;

FakeRelay::~FakeRelay() { Stop(); }

bool FakeRelay::Start(const std::vector<std::string>& extra_arguments) {
  std::string script = RepoRoot() + "/tools/fake_relay/relay.py";
  if (::access(script.c_str(), R_OK) != 0) return false;

  int pipe_fds[2];
  if (::pipe(pipe_fds) != 0) return false;

  pid_t pid = ::fork();
  if (pid < 0) {
    ::close(pipe_fds[0]);
    ::close(pipe_fds[1]);
    return false;
  }
  if (pid == 0) {
    ::close(pipe_fds[0]);
    ::dup2(pipe_fds[1], STDOUT_FILENO);
    ::close(pipe_fds[1]);
    std::vector<std::string> arguments = {"python3", script, "--port", "0"};
    for (const std::string& argument : extra_arguments) arguments.push_back(argument);
    std::vector<char*> argv;
    for (std::string& argument : arguments) argv.push_back(&argument[0]);
    argv.push_back(nullptr);
    ::execvp("python3", argv.data());
    ::_exit(127);
  }

  ::close(pipe_fds[1]);
  std::string line;
  char buffer[128];
  auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(10);
  while (std::chrono::steady_clock::now() < deadline) {
    ssize_t count = ::read(pipe_fds[0], buffer, sizeof(buffer));
    if (count <= 0) break;
    line.append(buffer, static_cast<std::size_t>(count));
    if (line.find('\n') != std::string::npos) break;
  }
  ::close(pipe_fds[0]);

  std::size_t position = line.find("LISTENING ");
  if (position == std::string::npos) {
    ::kill(pid, SIGTERM);
    ::waitpid(pid, nullptr, 0);
    return false;
  }
  std::uint64_t port = 0;
  std::string port_text = tmirror::Trim(line.substr(position + 10));
  if (!tmirror::ParseUint64(port_text, 65535, &port) || port == 0) {
    ::kill(pid, SIGTERM);
    ::waitpid(pid, nullptr, 0);
    return false;
  }
  pid_ = pid;
  port_ = static_cast<std::uint16_t>(port);
  return true;
}

void FakeRelay::Stop() {
  if (pid_ <= 0) return;
  ::kill(pid_, SIGTERM);
  int status = 0;
  ::waitpid(pid_, &status, 0);
  pid_ = -1;
  port_ = 0;
}

std::string FakeRelay::base_url() const {
  return "http://127.0.0.1:" + tmirror::Uint64ToString(port_);
}

Result<Json> FakeRelay::Control(const std::string& path, const Json& body,
                                const std::string& method) {
  tmirror::net::HttpClientConfig config;
  config.scheme = "http";
  config.host = "127.0.0.1";
  config.port = port_;
  config.request_timeout_ms = 10000;

  tmirror::net::HttpRequest request;
  request.method = method;
  request.target = path;
  request.body = method == "GET" ? std::string() : body.Serialize();
  request.content_type = "application/json";

  tmirror::net::HttpClient client(config);
  Result<tmirror::net::HttpResponse> response = client.Send(request);
  if (!response.ok()) return response.status();
  if (!response.value().ok()) {
    return Status::Error(tmirror::ErrorKind::kInternal,
                         "control call failed: " + response.value().body);
  }
  return Json::Parse(response.value().body);
}

std::string FakeRelay::CreateTerminal(const std::string& label, int columns, int rows,
                                      bool accepts_input) {
  Json body = Json::Object();
  body.Set("label", Json::String(label));
  body.Set("cols", Json::Int(columns));
  body.Set("rows", Json::Int(rows));
  body.Set("accepts_input", Json::Bool(accepts_input));
  Result<Json> response = Control("/_test/terminals", body);
  if (!response.ok()) return std::string();
  return response.value().GetString("terminal_id");
}

bool FakeRelay::Emit(const std::string& terminal_id, const std::string& text, int repeat) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("text", Json::String(text));
  body.Set("repeat", Json::Int(repeat));
  return Control("/_test/emit", body).ok();
}

bool FakeRelay::EmitBytes(const std::string& terminal_id, const std::string& raw_bytes) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("data_b64", Json::String(tmirror::Base64Encode(ByteView(raw_bytes))));
  return Control("/_test/emit", body).ok();
}

bool FakeRelay::SetDurable(const std::string& terminal_id, std::uint64_t offset) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("offset", Json::Uint(offset));
  return Control("/_test/durable", body).ok();
}

bool FakeRelay::Resize(const std::string& terminal_id, int columns, int rows) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("cols", Json::Int(columns));
  body.Set("rows", Json::Int(rows));
  return Control("/_test/resize", body).ok();
}

bool FakeRelay::Close(const std::string& terminal_id, const std::string& reason) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("reason", Json::String(reason));
  return Control("/_test/close", body).ok();
}

bool FakeRelay::Evict(const std::string& terminal_id, std::uint64_t bytes) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("bytes", Json::Uint(bytes));
  return Control("/_test/evict", body).ok();
}

bool FakeRelay::Drop(const std::string& terminal_id, int code) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("code", Json::Int(code));
  return Control("/_test/drop", body).ok();
}

bool FakeRelay::SetPolicy(const std::string& key, const Json& value) {
  Json body = Json::Object();
  body.Set(key, value);
  return Control("/_test/policy", body).ok();
}

bool FakeRelay::SetInputAvailable(const std::string& terminal_id, bool available) {
  Json body = Json::Object();
  body.Set("terminal_id", Json::String(terminal_id));
  body.Set("value", Json::Bool(available));
  return Control("/_test/input_available", body).ok();
}

std::string FakeRelay::ReceivedInput(const std::string& terminal_id) {
  Result<Json> response = Control("/_test/input/" + terminal_id, Json::Object(), "GET");
  if (!response.ok()) return std::string();
  const Json* frames = response.value().Find("frames");
  if (frames == nullptr || !frames->is_array()) return std::string();
  std::string text;
  for (const Json& frame : frames->items()) text += frame.GetString("text");
  return text;
}

int FakeRelay::ResizeRequestCount(const std::string& terminal_id) {
  Result<Json> response = Control("/_test/input/" + terminal_id, Json::Object(), "GET");
  if (!response.ok()) return 0;
  const Json* requests = response.value().Find("resize_requests");
  if (requests == nullptr || !requests->is_array()) return 0;
  return static_cast<int>(requests->items().size());
}

Result<FakeRelay::Owner> FakeRelay::CreateOwnerIdentity() {
  tmirror::api::RelayClientConfig config;
  Result<tmirror::net::Url> url = tmirror::net::ParseUrl(base_url());
  if (!url.ok()) return url.status();
  config.base_url = url.value();
  tmirror::api::RelayClient client(config);

  Result<tmirror::crypto::Ed25519KeyPair> identity_key =
      tmirror::crypto::Ed25519KeyPair::Generate();
  if (!identity_key.ok()) return identity_key.status();
  Result<std::string> identity_id = client.RegisterIdentityForKey(identity_key.value());
  if (!identity_id.ok()) return identity_id.status();
  Result<tmirror::api::AccessToken> token =
      client.AuthenticateIdentity(identity_key.value());
  if (!token.ok()) return token.status();

  Owner owner;
  owner.identity_id = identity_id.value();
  owner.identity_token = token.value().token;
  return owner;
}

Result<FakeRelay::PairedDevice> FakeRelay::PairClientDevice() {
  tmirror::api::RelayClientConfig config;
  Result<tmirror::net::Url> url = tmirror::net::ParseUrl(base_url());
  if (!url.ok()) return url.status();
  config.base_url = url.value();
  tmirror::api::RelayClient client(config);

  // 1. The owner's identity key, which never leaves that machine.
  Result<tmirror::crypto::Ed25519KeyPair> identity_key =
      tmirror::crypto::Ed25519KeyPair::Generate();
  if (!identity_key.ok()) return identity_key.status();
  Result<std::string> identity_id = client.RegisterIdentityForKey(identity_key.value());
  if (!identity_id.ok()) return identity_id.status();
  Result<tmirror::api::AccessToken> identity_token =
      client.AuthenticateIdentity(identity_key.value());
  if (!identity_token.ok()) return identity_token.status();

  // 2. The phone's own key. Only its public half and a signature cross the wire.
  Result<tmirror::crypto::Ed25519KeyPair> device_key =
      tmirror::crypto::Ed25519KeyPair::Generate();
  if (!device_key.ok()) return device_key.status();

  Result<tmirror::api::Challenge> challenge = client.CreateDeviceRegistrationChallenge(
      tmirror::crypto::kAlgorithmEd25519, ByteView(device_key.value().public_key()),
      identity_id.value());
  if (!challenge.ok()) return challenge.status();

  Result<tmirror::Bytes> signature =
      device_key.value().Sign(ByteView(challenge.value().signing_input));
  if (!signature.ok()) return signature.status();

  Result<tmirror::api::DeviceInfo> device = client.RegisterDevice(
      identity_token.value(), "integration test device", tmirror::crypto::kAlgorithmEd25519,
      ByteView(device_key.value().public_key()), challenge.value().challenge_id,
      ByteView(signature.value()), "client");
  if (!device.ok()) return device.status();

  PairedDevice paired;
  paired.identity_id = identity_id.value();
  paired.device_id = device.value().device_id;
  paired.device_seed = device_key.value().seed();
  return paired;
}

bool WaitFor(const std::function<bool()>& predicate, int timeout_ms) {
  auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
  while (std::chrono::steady_clock::now() < deadline) {
    if (predicate()) return true;
    std::this_thread::sleep_for(std::chrono::milliseconds(5));
  }
  return predicate();
}

}  // namespace tmtest
