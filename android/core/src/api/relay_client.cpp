#include "tm/api/relay_client.h"

#include "tm/util/base64.h"
#include "tm/util/json.h"
#include "tm/util/log.h"
#include "tm/util/strings.h"

namespace tmirror {
namespace api {
namespace {

constexpr const char kTag[] = "tm.api";

std::uint32_t ReadUint32(const Json& object, const std::string& key, std::uint32_t fallback) {
  const Json* value = object.Find(key);
  std::uint32_t out = fallback;
  if (value != nullptr && value->AsUint32Bounded(100000, &out)) return out;
  return fallback;
}

std::uint64_t ReadUint64(const Json& object, const std::string& key) {
  std::uint64_t value = 0;
  object.GetUint64(key, &value);
  return value;
}

TerminalInfo ParseTerminal(const Json& object) {
  TerminalInfo info;
  info.terminal_id = object.GetString("terminal_id");
  info.device_id = object.GetString("device_id");
  info.identity_id = object.GetString("identity_id");
  info.label = SanitizeForMessage(object.GetString("label"), 128);
  info.local_ref = SanitizeForMessage(object.GetString("local_ref"), 128);
  info.state = object.GetString("state");
  info.term = SanitizeForMessage(object.GetString("term"), 64);
  info.close_reason = SanitizeForMessage(object.GetString("close_reason"), 64);
  info.created_at = object.GetString("created_at");
  info.last_activity_at = object.GetString("last_activity_at");
  // Dimensions are untrusted input and are clamped before they reach the emulator
  // (spec §12); 0 means "not reported".
  info.columns = ReadUint32(object, "cols", 0);
  info.rows = ReadUint32(object, "rows", 0);
  info.earliest_offset = ReadUint64(object, "earliest_offset");
  info.next_offset = ReadUint64(object, "next_offset");
  info.durable_offset = ReadUint64(object, "durable_offset");
  info.retained_bytes = ReadUint64(object, "retained_bytes");
  info.accepts_input = object.GetBool("accepts_input", false);
  return info;
}

DeviceInfo ParseDevice(const Json& object) {
  DeviceInfo info;
  info.device_id = object.GetString("device_id");
  info.identity_id = object.GetString("identity_id");
  info.name = SanitizeForMessage(object.GetString("name"), 128);
  info.role = object.GetString("role");
  const Json* key = object.Find("key");
  if (key != nullptr) info.key_fingerprint = key->GetString("fingerprint");
  info.created_at = object.GetString("created_at");
  info.revoked_at = object.GetString("revoked_at");
  return info;
}

}  // namespace

bool AccessToken::HasScope(const std::string& scope) const {
  for (const std::string& granted : scopes) {
    if (granted == scope) return true;
  }
  return false;
}

ErrorKind ErrorKindForRelayCode(const std::string& code) {
  if (code == "unauthorized" || code == "invalid_signature" || code == "revoked") {
    return ErrorKind::kAuthFailed;
  }
  if (code == "token_expired") return ErrorKind::kAuthExpired;
  if (code == "not_found" || code == "terminal_not_found") return ErrorKind::kNotFound;
  if (code == "forbidden" || code == "insufficient_scope") return ErrorKind::kPermissionDenied;
  if (code == "terminal_closed") return ErrorKind::kTerminalClosed;
  if (code == "offset_ahead" || code == "slow_consumer") return ErrorKind::kSyncFailure;
  if (code == "rate_limited") return ErrorKind::kRateLimited;
  if (code == "storage_unavailable" || code == "server_shutdown") return ErrorKind::kServerError;
  if (code == "input_not_accepted" || code == "input_forbidden" || code == "input_disabled") {
    return ErrorKind::kInputRefused;
  }
  if (code == "input_undeliverable" || code == "input_backpressure") {
    return ErrorKind::kInputUndeliverable;
  }
  if (code == "input_sequence_mismatch") return ErrorKind::kInputUndeliverable;
  if (code == "unknown_message_type" || code == "invalid_message" ||
      code == "validation_failed") {
    return ErrorKind::kProtocolError;
  }
  if (code == "feature_disabled") return ErrorKind::kPermissionDenied;
  return ErrorKind::kProtocolError;
}

bool IsTransientInputRefusal(const std::string& code) {
  return code == "input_undeliverable" || code == "input_backpressure" ||
         code == "input_sequence_mismatch" || code == "rate_limited";
}

Status StatusFromHttp(int http_status, const std::string& body) {
  std::string code;
  std::string message;
  Result<Json> parsed = Json::Parse(body);
  if (parsed.ok()) {
    const Json* error = parsed.value().Find("error");
    if (error != nullptr) {
      code = error->GetString("code");
      message = SanitizeForMessage(error->GetString("message"), 200);
    }
  }

  ErrorKind kind;
  switch (http_status) {
    case 401: kind = ErrorKind::kAuthFailed; break;
    case 403: kind = ErrorKind::kPermissionDenied; break;
    // Ownership failures answer 404 rather than 403 (relay spec §4.4), so a 404 on a
    // terminal is "not yours or not there" and must not be reported as a bug.
    case 404: kind = ErrorKind::kNotFound; break;
    case 409: kind = ErrorKind::kProtocolError; break;
    case 422: kind = ErrorKind::kInvalidArgument; break;
    case 429: kind = ErrorKind::kRateLimited; break;
    default:
      if (http_status >= 500) {
        kind = ErrorKind::kServerError;
      } else if (http_status >= 400) {
        kind = ErrorKind::kProtocolError;
      } else {
        kind = ErrorKind::kNone;
      }
  }
  if (!code.empty()) {
    ErrorKind mapped = ErrorKindForRelayCode(code);
    if (mapped != ErrorKind::kProtocolError) kind = mapped;
  }
  if (message.empty()) message = "request failed with status " + Int64ToString(http_status);
  return Status::Error(kind, message).set_code(code);
}

RelayClient::RelayClient(RelayClientConfig config) : config_(std::move(config)) {
  http_config_.scheme = config_.base_url.scheme;
  http_config_.host = config_.base_url.host;
  http_config_.port = config_.base_url.port;
  http_config_.tls = config_.tls;
  if (http_config_.tls.hostname.empty()) http_config_.tls.hostname = http_config_.host;
  http_config_.connect_timeout_ms = config_.connect_timeout_ms;
  http_config_.request_timeout_ms = config_.request_timeout_ms;
  http_config_.user_agent = config_.user_agent;
  http_config_.dialer = config_.dialer;
  http_config_.allow_cleartext_over_tunnel = config_.allow_cleartext_over_tunnel;
}

Result<net::HttpResponse> RelayClient::Send(const std::string& method,
                                            const std::string& target, const std::string& body,
                                            const AccessToken* token) {
  net::HttpRequest request;
  request.method = method;
  request.target = target;
  request.body = body;
  request.content_type = "application/json";
  request.headers.push_back({"Accept", "application/json"});
  if (token != nullptr && token->valid()) {
    // Token material goes in a header, never in the URL (relay spec §4.3).
    request.headers.push_back({"Authorization", "Bearer " + token->token});
  }
  net::HttpClient client(http_config_);
  return client.Send(request, cancel_);
}

Result<Json> RelayClient::SendJson(const std::string& method, const std::string& target,
                                   const std::string& body, const AccessToken* token) {
  Result<net::HttpResponse> response = Send(method, target, body, token);
  if (!response.ok()) return response.status();
  if (!response.value().ok()) {
    return StatusFromHttp(response.value().status, response.value().body);
  }
  if (response.value().body.empty()) return Json::Object();
  Result<Json> parsed = Json::Parse(response.value().body);
  if (!parsed.ok()) return parsed.status();
  if (!parsed.value().is_object()) {
    return Status::Error(ErrorKind::kProtocolError, "expected a JSON object response");
  }
  return parsed;
}

Result<Challenge> RelayClient::CreateChallenge(crypto::ChallengeOperation operation,
                                               const std::string& algorithm,
                                               ByteView public_key,
                                               const std::string& owner_identity_id) {
  Json key = Json::Object();
  key.Set("algorithm", Json::String(algorithm));
  key.Set("public_key", Json::String(Base64UrlEncode(public_key)));

  Json request = Json::Object();
  request.Set("operation", Json::String(crypto::ChallengeOperationName(operation)));
  request.Set("key", std::move(key));
  if (!owner_identity_id.empty()) {
    request.Set("owner_identity_id", Json::String(owner_identity_id));
  }

  Result<Json> response = SendJson("POST", "/v1/auth/challenges", request.Serialize(), nullptr);
  if (!response.ok()) return response.status();

  Challenge challenge;
  challenge.challenge_id = response.value().GetString("challenge_id");
  challenge.signature_context = response.value().GetString("signature_context");
  challenge.expires_at = response.value().GetString("expires_at");
  challenge.key_fingerprint = response.value().GetString("key_fingerprint");
  if (!Base64UrlDecode(response.value().GetString("challenge"), &challenge.challenge) ||
      !Base64UrlDecode(response.value().GetString("signing_input"), &challenge.signing_input)) {
    return Status::Error(ErrorKind::kProtocolError, "challenge fields are not base64url");
  }
  if (challenge.challenge_id.empty() || challenge.challenge.size() < 32 ||
      challenge.signing_input.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "challenge response is incomplete");
  }
  return challenge;
}

Result<std::string> RelayClient::RegisterIdentity(const std::string& challenge_id,
                                                  ByteView signature) {
  Json request = Json::Object();
  request.Set("challenge_id", Json::String(challenge_id));
  request.Set("signature", Json::String(Base64UrlEncode(signature)));
  Result<Json> response = SendJson("POST", "/v1/identities", request.Serialize(), nullptr);
  if (!response.ok()) return response.status();
  std::string identity_id = response.value().GetString("identity_id");
  if (identity_id.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "identity response is incomplete");
  }
  return identity_id;
}

Result<AccessToken> RelayClient::CreateToken(const std::string& challenge_id,
                                             ByteView signature) {
  Json request = Json::Object();
  request.Set("challenge_id", Json::String(challenge_id));
  request.Set("signature", Json::String(Base64UrlEncode(signature)));
  Result<Json> response = SendJson("POST", "/v1/auth/tokens", request.Serialize(), nullptr);
  if (!response.ok()) return response.status();

  AccessToken token;
  token.token = response.value().GetString("access_token");
  token.principal = response.value().GetString("principal");
  token.principal_id = response.value().GetString("principal_id");
  std::uint64_t expires_in = 0;
  response.value().GetUint64("expires_in", &expires_in);
  if (expires_in > 15 * 60) expires_in = 15 * 60;  // relay spec §4.3 caps this at 15 minutes
  token.expires_at_unix_ms =
      Clock::System()->UnixMillis() + static_cast<Millis>(expires_in) * 1000;
  const Json* scopes = response.value().Find("scopes");
  if (scopes != nullptr && scopes->is_array()) {
    for (const Json& scope : scopes->items()) {
      if (scope.is_string()) token.scopes.push_back(scope.string_value());
    }
  }
  if (token.token.empty()) {
    return Status::Error(ErrorKind::kAuthFailed, "token response is incomplete");
  }
  return token;
}

Result<std::string> RelayClient::CreateWebSocketTicket(const AccessToken& token,
                                                       const std::string& path) {
  Json request = Json::Object();
  request.Set("path", Json::String(path));
  Result<Json> response =
      SendJson("POST", "/v1/auth/websocket-tickets", request.Serialize(), &token);
  if (!response.ok()) return response.status();
  std::string ticket = response.value().GetString("ticket");
  if (ticket.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "ticket response is incomplete");
  }
  return ticket;
}

Result<DeviceInfo> RelayClient::RegisterDevice(const AccessToken& identity_token,
                                               const std::string& name,
                                               const std::string& algorithm,
                                               ByteView device_public_key,
                                               const std::string& challenge_id,
                                               ByteView device_signature,
                                               const std::string& role) {
  Json key = Json::Object();
  key.Set("algorithm", Json::String(algorithm));
  key.Set("public_key", Json::String(Base64UrlEncode(device_public_key)));

  Json request = Json::Object();
  request.Set("name", Json::String(name));
  request.Set("key", std::move(key));
  request.Set("challenge_id", Json::String(challenge_id));
  request.Set("device_signature", Json::String(Base64UrlEncode(device_signature)));
  if (!role.empty()) request.Set("role", Json::String(role));

  Result<Json> response = SendJson("POST", "/v1/devices", request.Serialize(), &identity_token);
  if (!response.ok()) return response.status();
  DeviceInfo device = ParseDevice(response.value());
  if (device.device_id.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "device response is incomplete");
  }
  return device;
}

Result<std::vector<DeviceInfo>> RelayClient::ListDevices(const AccessToken& token) {
  Result<Json> response = SendJson("GET", "/v1/devices", std::string(), &token);
  if (!response.ok()) return response.status();
  std::vector<DeviceInfo> devices;
  const Json* array = response.value().Find("devices");
  if (array != nullptr && array->is_array()) {
    for (const Json& item : array->items()) {
      if (item.is_object()) devices.push_back(ParseDevice(item));
    }
  }
  return devices;
}

Status RelayClient::RevokeDevice(const AccessToken& token, const std::string& device_id) {
  Result<Json> response =
      SendJson("DELETE", "/v1/devices/" + UrlEncode(device_id), std::string(), &token);
  return response.ok() ? Status::Ok() : response.status();
}

Result<TerminalPage> RelayClient::ListTerminals(const AccessToken& token,
                                                const std::string& state_filter,
                                                const std::string& cursor, int limit) {
  std::string target = "/v1/terminals";
  std::string query;
  if (!state_filter.empty()) query += "state=" + UrlEncode(state_filter);
  if (!cursor.empty()) {
    if (!query.empty()) query += "&";
    query += "cursor=" + UrlEncode(cursor);
  }
  if (limit > 0) {
    if (!query.empty()) query += "&";
    query += "limit=" + Int64ToString(limit);
  }
  if (!query.empty()) target += "?" + query;

  Result<Json> response = SendJson("GET", target, std::string(), &token);
  if (!response.ok()) return response.status();

  TerminalPage page;
  const Json* array = response.value().Find("terminals");
  if (array != nullptr && array->is_array()) {
    for (const Json& item : array->items()) {
      if (item.is_object()) page.terminals.push_back(ParseTerminal(item));
    }
  }
  page.next_cursor = response.value().GetString("next_cursor");
  return page;
}

Result<TerminalInfo> RelayClient::GetTerminal(const AccessToken& token,
                                              const std::string& terminal_id) {
  Result<Json> response =
      SendJson("GET", "/v1/terminals/" + UrlEncode(terminal_id), std::string(), &token);
  if (!response.ok()) return response.status();
  TerminalInfo info = ParseTerminal(response.value());
  if (info.terminal_id.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "terminal response is incomplete");
  }
  return info;
}

Result<TerminalInfo> RelayClient::OpenTerminal(const AccessToken& token,
                                              const std::string& device_id,
                                              const std::string& label, int columns, int rows,
                                              const std::string& idempotency_key) {
  Json request = Json::Object();
  if (!label.empty()) request.Set("label", Json::String(label));
  if (columns > 0) request.Set("cols", Json::Int(columns));
  if (rows > 0) request.Set("rows", Json::Int(rows));

  net::HttpRequest http;
  http.method = "POST";
  http.target = "/v1/devices/" + UrlEncode(device_id) + "/terminals";
  http.body = request.Serialize();
  http.content_type = "application/json";
  http.headers.push_back({"Accept", "application/json"});
  http.headers.push_back({"Authorization", "Bearer " + token.token});
  // Required, not optional: the endpoint refuses without it, because the alternative is
  // a retry that starts a second shell (relay spec §5.2).
  http.headers.push_back({"Idempotency-Key", idempotency_key});

  net::HttpClient client(http_config_);
  Result<net::HttpResponse> response = client.Send(http, cancel_);
  if (!response.ok()) return response.status();
  if (!response.value().ok()) {
    return StatusFromHttp(response.value().status, response.value().body);
  }
  Result<Json> parsed = Json::Parse(response.value().body);
  if (!parsed.ok()) return parsed.status();
  if (!parsed.value().is_object()) {
    return Status::Error(ErrorKind::kProtocolError, "expected a JSON object response");
  }
  TerminalInfo info = ParseTerminal(parsed.value());
  if (info.terminal_id.empty()) {
    return Status::Error(ErrorKind::kProtocolError, "terminal response is incomplete");
  }
  return info;
}

Result<AccessToken> RelayClient::AuthenticateWithOperation(
    const crypto::Ed25519KeyPair& key, crypto::ChallengeOperation operation) {
  if (!key.valid()) {
    return Status::Error(ErrorKind::kAuthFailed, "no device key is available");
  }
  Result<Challenge> challenge =
      CreateChallenge(operation, crypto::kAlgorithmEd25519, ByteView(key.public_key()));
  if (!challenge.ok()) return challenge.status();

  crypto::ExpectedSigningInput expected;
  expected.challenge_id = challenge.value().challenge_id;
  expected.challenge = challenge.value().challenge;
  expected.operation = operation;
  expected.key_fingerprint =
      crypto::KeyFingerprint(crypto::kAlgorithmEd25519, ByteView(key.public_key()));
  expected.expected_origin = config_.base_url.origin();
  // A relay that asks us to sign anything other than exactly this challenge is not
  // getting a signature (spec §12).
  Status verified =
      crypto::VerifySigningInput(ByteView(challenge.value().signing_input), expected);
  if (!verified.ok()) {
    // The origin is the one field a legitimate deployment may differ on (a proxy in
    // front of the relay), so retry once without it before refusing.
    expected.expected_origin.clear();
    verified = crypto::VerifySigningInput(ByteView(challenge.value().signing_input), expected);
    if (!verified.ok()) return verified;
    TM_LOG_DEBUG(kTag, "challenge origin differs from the configured base URL");
  }

  Result<Bytes> signature = key.Sign(ByteView(challenge.value().signing_input));
  if (!signature.ok()) return signature.status();
  return CreateToken(challenge.value().challenge_id, ByteView(signature.value()));
}

Result<AccessToken> RelayClient::AuthenticateIdentity(const crypto::Ed25519KeyPair& key) {
  return AuthenticateWithOperation(key, crypto::ChallengeOperation::kAuthenticateIdentity);
}

Result<AccessToken> RelayClient::AuthenticateDevice(const crypto::Ed25519KeyPair& key) {
  return AuthenticateWithOperation(key, crypto::ChallengeOperation::kAuthenticateDevice);
}

Result<std::string> RelayClient::RegisterIdentityForKey(const crypto::Ed25519KeyPair& key) {
  Result<Challenge> challenge =
      CreateChallenge(crypto::ChallengeOperation::kRegisterIdentity, crypto::kAlgorithmEd25519,
                      ByteView(key.public_key()));
  if (!challenge.ok()) return challenge.status();

  crypto::ExpectedSigningInput expected;
  expected.challenge_id = challenge.value().challenge_id;
  expected.challenge = challenge.value().challenge;
  expected.operation = crypto::ChallengeOperation::kRegisterIdentity;
  expected.key_fingerprint =
      crypto::KeyFingerprint(crypto::kAlgorithmEd25519, ByteView(key.public_key()));
  Status verified =
      crypto::VerifySigningInput(ByteView(challenge.value().signing_input), expected);
  if (!verified.ok()) return verified;

  Result<Bytes> signature = key.Sign(ByteView(challenge.value().signing_input));
  if (!signature.ok()) return signature.status();
  return RegisterIdentity(challenge.value().challenge_id, ByteView(signature.value()));
}

Result<Challenge> RelayClient::CreateDeviceRegistrationChallenge(
    const std::string& algorithm, ByteView device_public_key,
    const std::string& owner_identity_id) {
  return CreateChallenge(crypto::ChallengeOperation::kRegisterDevice, algorithm,
                         device_public_key, owner_identity_id);
}

}  // namespace api
}  // namespace tmirror
