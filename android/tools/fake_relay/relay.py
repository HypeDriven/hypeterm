#!/usr/bin/env python3
"""Fake Terminal Mirror Relay.

Client development must not depend on the production relay (client spec §16.3), so
this implements the normalized behaviour of `../server/spec.md`: proof-of-possession
challenges, tokens, terminal discovery, the mirror subprotocol, and — crucially — its
failure paths, because those are the ones that are hard to reach against a healthy
server: `gap`, `offset_ahead`, `slow_consumer`, `terminal.closed`, token expiry and
every input refusal in relay spec §6.3.

Everything a test needs to steer is exposed under `/_test/`. That surface is not part
of the relay contract and exists only here.

Run:  python3 relay.py --port 0 --state-file /tmp/state.json
It prints `LISTENING <port>` on stdout once ready.
"""

import argparse
import base64
import hashlib
import json
import os
import socket
import ssl
import struct
import sys
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ed25519  # noqa: E402

CHALLENGE_CONTEXT = b"terminal-relay-challenge-v1"
IDENTITY_CONTEXT = b"terminal-relay-identity-v1"
WS_GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
REPLAY_CAPACITY = 1_500_000
MIRROR_V1 = "terminal-relay.mirror.v1"
MIRROR_V2 = "terminal-relay.mirror.v2"


def b64url_encode(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).decode("ascii").rstrip("=")


def b64url_decode(text: str) -> bytes:
    padding = "=" * (-len(text) % 4)
    return base64.urlsafe_b64decode(text + padding)


def length_prefixed(*fields: bytes) -> bytes:
    out = b""
    for field in fields:
        out += struct.pack("!I", len(field)) + field
    return out


def fingerprint(algorithm: str, public_key: bytes) -> str:
    digest = hashlib.sha256(
        length_prefixed(IDENTITY_CONTEXT, algorithm.encode(), public_key)
    ).digest()
    return b64url_encode(digest)


def now_ms() -> int:
    return int(time.time() * 1000)


def rfc3339(ms: int) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(ms / 1000)) + ".000000Z"


DEFAULT_DEVICE_ID = "00000000-0000-4000-8000-000000000001"


class Terminal:
    def __init__(self, terminal_id, label, cols, rows, term, accepts_input,
                 device_id=DEFAULT_DEVICE_ID):
        self.id = terminal_id
        self.device_id = device_id
        self.label = label
        self.cols = cols
        self.rows = rows
        self.term = term
        self.accepts_input = accepts_input
        self.input_available = accepts_input
        self.state = "open"
        self.close_reason = None
        self.buffer = bytearray()
        self.earliest_offset = 0
        self.next_offset = 0
        self.durable_offset = 0
        self.subscribers = []
        self.received_input = []
        self.resize_requests = []
        self.lock = threading.Lock()

    def append(self, data: bytes):
        with self.lock:
            self.buffer += data
            self.next_offset += len(data)
            if len(self.buffer) > REPLAY_CAPACITY:
                drop = len(self.buffer) - REPLAY_CAPACITY
                del self.buffer[:drop]
                self.earliest_offset += drop
            subscribers = list(self.subscribers)
        start = self.next_offset - len(data)
        for subscriber in subscribers:
            subscriber.send_output(start, data)

    def slice_from(self, offset: int) -> bytes:
        with self.lock:
            if offset < self.earliest_offset or offset > self.next_offset:
                return b""
            return bytes(self.buffer[offset - self.earliest_offset:])

    def json(self):
        return {
            "terminal_id": self.id,
            # Real, not a constant: a client that asks a *device* for a terminal has to
            # be able to learn which device a terminal it can see belongs to.
            "device_id": self.device_id,
            "identity_id": "fake-identity",
            "label": self.label,
            "local_ref": "pty0",
            "state": self.state,
            "cols": self.cols,
            "rows": self.rows,
            "term": self.term,
            "created_at": rfc3339(now_ms()),
            "last_activity_at": rfc3339(now_ms()),
            "closed_at": None,
            "close_reason": self.close_reason,
            "accepts_input": self.accepts_input,
            "earliest_offset": self.earliest_offset,
            "next_offset": self.next_offset,
            "durable_offset": self.durable_offset,
            "retained_bytes": self.next_offset - self.earliest_offset,
        }


class State:
    def __init__(self, options):
        self.options = options
        self.lock = threading.Lock()
        self.identities = {}
        self.devices = {}
        self.challenges = {}
        self.tokens = {}
        self.tickets = {}
        self.terminals = {}
        # Devices that answer "no capable publisher", for exercising that path.
        self.refuse_open = set()
        # Test-controlled behaviour switches.
        self.policy = {
            "token_ttl_seconds": options.token_ttl,
            "force_offset_ahead": False,
            "force_slow_consumer": False,
            "reject_upgrade_status": 0,
            "input_error": "",
            "heartbeat_interval_seconds": 20,
            "heartbeat_timeout_seconds": 60,
            "max_input_frame_bytes": 4096,
            "offer_v1_only": False,
            # A cooperative publisher applies the sizes subscribers ask for. Turn this
            # off to exercise a publisher that declines (relay spec §6.3).
            "auto_apply_resize": True,
        }

    def pairing_code(self, origin):
        """An owner identity and a short-lived token, encoded the way the publisher's
        `pair-code` encodes them (client spec §5.1, core/src/api/pairing.cpp).

        The manual two-field flow the app used to offer only ever worked here, because
        the real relay requires a device to sign its own registration. Minting a code
        instead means development pairs by the same path a person does.
        """
        identity_id = "fake-owner-identity"
        self.identities.setdefault(identity_id, "fake-owner-key")
        token = "v1." + b64url_encode(os.urandom(24))
        self.tokens[token] = {
            "principal": "identity",
            "principal_id": identity_id,
            "identity_id": identity_id,
            "scopes": ["devices:read", "devices:write", "terminals:read",
                       "terminals:mirror", "terminals:input", "terminals:create"],
            "expires_at_ms": now_ms() + 15 * 60 * 1000,
        }
        payload = json.dumps({"u": origin, "i": identity_id, "t": token}).encode()
        return "HT1." + b64url_encode(payload)

    def new_terminal(self, label="shell", cols=80, rows=24, term="xterm-256color",
                     accepts_input=True, device_id=DEFAULT_DEVICE_ID):
        terminal = Terminal(str(uuid.uuid4()), label, cols, rows, term, accepts_input,
                            device_id)
        with self.lock:
            self.terminals[terminal.id] = terminal
        return terminal


class MirrorConnection:
    """One mirror WebSocket. Owns its socket's write side."""

    def __init__(self, handler, terminal, state, version):
        self.handler = handler
        self.terminal = terminal
        self.state = state
        self.version = version
        self.lock = threading.Lock()
        self.closed = False
        self.expected_sequence = 1
        self.accepted_through = 0
        self.relay_sequence = 0
        self.subscribed = False
        self.next_offset_sent = 0

    # -- framing ----------------------------------------------------------
    def _send_frame(self, opcode: int, payload: bytes):
        with self.lock:
            if self.closed:
                return
            header = bytearray()
            header.append(0x80 | opcode)
            length = len(payload)
            if length < 126:
                header.append(length)
            elif length <= 0xFFFF:
                header.append(126)
                header += struct.pack("!H", length)
            else:
                header.append(127)
                header += struct.pack("!Q", length)
            try:
                self.handler.connection.sendall(bytes(header) + payload)
            except OSError:
                self.closed = True

    def send_text(self, message: dict):
        self._send_frame(0x1, json.dumps(message).encode())

    def send_output(self, start: int, data: bytes):
        if not self.subscribed or not data:
            return
        if self.state.policy["force_slow_consumer"]:
            self.send_text({"type": "error", "code": "slow_consumer",
                            "message": "subscriber outbound queue exceeded"})
            self.close(4003)
            return
        self._send_frame(0x2, b"\x01" + struct.pack("!Q", start) + data)
        self.next_offset_sent = start + len(data)

    def send_ping(self):
        self._send_frame(0x9, b"")

    def close(self, code=1000, reason=b""):
        with self.lock:
            if self.closed:
                return
            self.closed = True
            try:
                self.handler.connection.sendall(
                    bytes([0x88, 2 + len(reason)]) + struct.pack("!H", code) + reason)
            except OSError:
                pass


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "FakeTerminalRelay/1"

    # Quiet by default: the tests read stdout for the port line.
    def log_message(self, fmt, *args):
        if self.server.state.options.verbose:
            sys.stderr.write("relay: " + (fmt % args) + "\n")

    # -- helpers ----------------------------------------------------------
    @property
    def state(self) -> State:
        return self.server.state

    def read_json(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        if length == 0:
            return {}
        try:
            return json.loads(self.rfile.read(length).decode())
        except (ValueError, UnicodeDecodeError):
            return None

    def send_json(self, status: int, payload: dict, extra_headers=()):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for name, value in extra_headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(body)

    def send_error_envelope(self, status: int, code: str, message: str):
        self.send_json(status, {"error": {"code": code, "message": message,
                                          "request_id": str(uuid.uuid4())}})

    def authenticated(self):
        header = self.headers.get("Authorization", "")
        if not header.lower().startswith("bearer "):
            return None
        token = header[7:].strip()
        record = self.state.tokens.get(token)
        if record is None:
            return None
        if record["expires_at_ms"] < now_ms():
            return None
        return record

    # -- routing ----------------------------------------------------------
    def do_POST(self):
        path = self.path.split("?")[0]
        body = self.read_json()
        if body is None:
            return self.send_error_envelope(400, "invalid_request", "body is not JSON")

        if path == "/v1/auth/challenges":
            return self.create_challenge(body)
        if path == "/v1/identities":
            return self.register_identity(body)
        if path == "/v1/auth/tokens":
            return self.create_token(body)
        if path == "/v1/auth/websocket-tickets":
            return self.create_ticket(body)
        if path == "/v1/devices":
            return self.register_device(body)
        if path.startswith("/v1/devices/") and path.endswith("/terminals"):
            return self.create_terminal_for_device(path, body)
        if path.startswith("/_test/"):
            return self.test_control(path, body)
        return self.send_error_envelope(404, "not_found", "no such resource")

    def create_terminal_for_device(self, path, body):
        """Asking a device to open a terminal (relay spec §4.6).

        There is no publisher here to forward to, so the answer is synthesised. What is
        worth exercising client-side is the shape of the exchange: the key is required,
        unknown fields are refused, and the terminal that comes back belongs to the
        device that was asked.
        """
        if not self.headers.get("Idempotency-Key"):
            return self.send_error_envelope(
                400, "idempotency_key_required", "Idempotency-Key is required")
        allowed = {"label", "cols", "rows"}
        unknown = set(body) - allowed
        if unknown:
            # Never ignored: this is what stops a command field being smuggled in.
            return self.send_error_envelope(
                400, "invalid_request", f"unknown field {sorted(unknown)[0]}")
        device_id = path.split("/")[3]
        if device_id in self.state.refuse_open:
            return self.send_error_envelope(
                503, "publisher_unavailable", "that device is not accepting requests")
        terminal = self.state.new_terminal(
            label=body.get("label") or "phone",
            cols=int(body.get("cols") or 80),
            rows=int(body.get("rows") or 24),
            device_id=device_id,
        )
        payload = terminal.json()
        payload["deduplicated"] = False
        self.send_json(201, payload, extra_headers=[("Location", f"/v1/terminals/{terminal.id}")])

    def do_GET(self):
        path = self.path.split("?")[0]
        if path.startswith("/v1/terminals/") and path.endswith("/mirror"):
            return self.mirror_upgrade(path)
        if path == "/v1/terminals":
            return self.list_terminals()
        if path.startswith("/v1/terminals/"):
            return self.get_terminal(path.rsplit("/", 1)[-1])
        if path == "/v1/devices":
            return self.list_devices()
        if path == "/healthz":
            return self.send_json(200, {"status": "ok"})
        if path.startswith("/_test/"):
            return self.test_control(path, {})
        return self.send_error_envelope(404, "not_found", "no such resource")

    def do_DELETE(self):
        path = self.path.split("?")[0]
        if path.startswith("/v1/devices/"):
            device_id = path.rsplit("/", 1)[-1]
            self.state.devices.pop(device_id, None)
            return self.send_json(200, {"device_id": device_id,
                                        "revoked_at": rfc3339(now_ms())})
        return self.send_error_envelope(404, "not_found", "no such resource")

    # -- authentication ---------------------------------------------------
    def create_challenge(self, body):
        operation = body.get("operation", "")
        if operation not in ("register_identity", "authenticate_identity",
                             "register_device", "authenticate_device"):
            return self.send_error_envelope(422, "validation_failed", "unknown operation")
        key = body.get("key") or {}
        algorithm = str(key.get("algorithm", "")).lower()
        if algorithm != "ed25519":
            return self.send_error_envelope(422, "unsupported_algorithm", "unsupported key")
        try:
            public_key = b64url_decode(str(key.get("public_key", "")))
        except Exception:
            return self.send_error_envelope(400, "invalid_request", "public_key is not base64url")
        if len(public_key) != 32:
            return self.send_error_envelope(422, "validation_failed", "bad key length")

        challenge_id = str(uuid.uuid4())
        challenge = os.urandom(32)
        expires_at = now_ms() + 300_000
        key_fingerprint = fingerprint(algorithm, public_key)
        owner = str(body.get("owner_identity_id") or "")
        device_fingerprint = key_fingerprint if operation == "register_device" else ""
        signing_input = length_prefixed(
            CHALLENGE_CONTEXT,
            self.state.options.origin.encode(),
            challenge_id.encode(),
            challenge,
            operation.encode(),
            key_fingerprint.encode(),
            owner.encode(),
            device_fingerprint.encode(),
            struct.pack("!Q", expires_at),
        )
        with self.state.lock:
            self.state.challenges[challenge_id] = {
                "operation": operation,
                "algorithm": algorithm,
                "public_key": public_key,
                "signing_input": signing_input,
                "expires_at_ms": expires_at,
                "fingerprint": key_fingerprint,
                "owner_identity_id": owner,
            }
        return self.send_json(201, {
            "challenge_id": challenge_id,
            "challenge": b64url_encode(challenge),
            "signature_context": CHALLENGE_CONTEXT.decode(),
            "signing_input": b64url_encode(signing_input),
            "expires_at": rfc3339(expires_at),
            "key_fingerprint": key_fingerprint,
        })

    def claim_challenge(self, body, expected_operations):
        challenge_id = str(body.get("challenge_id", ""))
        signature_field = "device_signature" if "device_signature" in body else "signature"
        try:
            signature = b64url_decode(str(body.get(signature_field, "")))
        except Exception:
            return None, self.send_error_envelope(400, "invalid_request", "bad signature")
        with self.state.lock:
            # A challenge is consumed by the attempt itself, success or failure.
            record = self.state.challenges.pop(challenge_id, None)
        if record is None:
            return None, self.send_error_envelope(401, "unauthorized", "unknown challenge")
        if record["expires_at_ms"] < now_ms():
            return None, self.send_error_envelope(401, "unauthorized", "challenge expired")
        if record["operation"] not in expected_operations:
            return None, self.send_error_envelope(401, "unauthorized", "wrong operation")
        if not ed25519.verify(record["public_key"], record["signing_input"], signature):
            return None, self.send_error_envelope(401, "unauthorized", "invalid signature")
        return record, None

    def register_identity(self, body):
        record, error = self.claim_challenge(body, ("register_identity",))
        if record is None:
            return error
        identity_id = record["fingerprint"]
        created = identity_id not in self.state.identities
        self.state.identities[identity_id] = record["public_key"]
        return self.send_json(201 if created else 200,
                              {"identity_id": identity_id, "created_at": rfc3339(now_ms())})

    def register_device(self, body):
        auth = self.authenticated()
        if auth is None:
            return self.send_error_envelope(401, "unauthorized", "identity token required")
        record, error = self.claim_challenge(body, ("register_device",))
        if record is None:
            return error
        # The challenge is bound to the identity that will own the device, and the
        # request is authorised by a token. Both must name the same identity, or a
        # token for one identity could enrol a device the challenge promised to
        # another (spec §4.4, §5.2).
        if record.get("owner_identity_id") != auth["identity_id"]:
            return self.send_error_envelope(
                403, "forbidden",
                "the challenge is bound to a different identity than the token")
        device_id = str(uuid.uuid4())
        role = body.get("role") or "publisher"
        device = {
            "device_id": device_id,
            "identity_id": auth["identity_id"],
            "name": body.get("name", "device"),
            "role": role,
            "fingerprint": record["fingerprint"],
            "public_key": record["public_key"],
        }
        self.state.devices[device_id] = device
        return self.send_json(201, {
            "device_id": device_id,
            "identity_id": device["identity_id"],
            "name": device["name"],
            "role": role,
            "key": {"algorithm": "ed25519", "fingerprint": device["fingerprint"]},
            "created_at": rfc3339(now_ms()),
            "last_seen_at": None,
            "revoked_at": None,
        })

    def create_token(self, body):
        record, error = self.claim_challenge(
            body, ("authenticate_identity", "authenticate_device"))
        if record is None:
            return error

        if record["operation"] == "authenticate_identity":
            identity_id = record["fingerprint"]
            if identity_id not in self.state.identities:
                return self.send_error_envelope(401, "unauthorized", "key is not registered")
            principal, principal_id = "identity", identity_id
            scopes = ["devices:read", "devices:write", "terminals:read",
                      "terminals:mirror", "terminals:input", "terminals:create"]
        else:
            device = None
            for candidate in self.state.devices.values():
                if candidate["fingerprint"] == record["fingerprint"]:
                    device = candidate
                    break
            if device is None:
                return self.send_error_envelope(401, "unauthorized", "device is not registered")
            principal, principal_id = "device", device["device_id"]
            identity_id = device["identity_id"]
            if device["role"] == "publisher":
                scopes = ["terminals:write", "terminals:publish"]
            else:
                scopes = ["terminals:read", "terminals:mirror", "terminals:input",
                          "terminals:create", "devices:read"]

        ttl = int(self.state.policy["token_ttl_seconds"])
        token = "v1." + b64url_encode(os.urandom(24))
        self.state.tokens[token] = {
            "principal": principal,
            "principal_id": principal_id,
            "identity_id": identity_id,
            "scopes": scopes,
            "expires_at_ms": now_ms() + ttl * 1000,
        }
        return self.send_json(200, {
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": ttl,
            "scopes": scopes,
            "principal": principal,
            "principal_id": principal_id,
        })

    def create_ticket(self, body):
        auth = self.authenticated()
        if auth is None:
            return self.send_error_envelope(401, "unauthorized", "token required")
        path = str(body.get("path", ""))
        ticket = b64url_encode(os.urandom(24))
        self.state.tickets[ticket] = {"path": path, "expires_at_ms": now_ms() + 60_000,
                                      "auth": auth}
        return self.send_json(201, {"ticket": ticket, "path": path,
                                    "expires_at": rfc3339(now_ms() + 60_000)})

    # -- resources --------------------------------------------------------
    def list_terminals(self):
        if self.authenticated() is None:
            return self.send_error_envelope(401, "unauthorized", "token required")
        terminals = [t.json() for t in self.state.terminals.values()]
        return self.send_json(200, {"terminals": terminals, "next_cursor": None})

    def get_terminal(self, terminal_id):
        if self.authenticated() is None:
            return self.send_error_envelope(401, "unauthorized", "token required")
        terminal = self.state.terminals.get(terminal_id)
        if terminal is None:
            # Ownership failures answer 404, never 403 (relay spec §4.4).
            return self.send_error_envelope(404, "not_found", "no such terminal")
        return self.send_json(200, terminal.json())

    def list_devices(self):
        if self.authenticated() is None:
            return self.send_error_envelope(401, "unauthorized", "token required")
        devices = [{
            "device_id": d["device_id"],
            "identity_id": d["identity_id"],
            "name": d["name"],
            "role": d["role"],
            "key": {"algorithm": "ed25519", "fingerprint": d["fingerprint"]},
            "created_at": rfc3339(now_ms()),
            "last_seen_at": None,
            "revoked_at": None,
        } for d in self.state.devices.values()]
        return self.send_json(200, {"devices": devices, "next_cursor": None})

    # -- test control -----------------------------------------------------
    def test_control(self, path, body):
        parts = path.strip("/").split("/")
        action = parts[1] if len(parts) > 1 else ""

        if action == "terminals":
            terminal = self.state.new_terminal(
                label=body.get("label", "shell"),
                cols=int(body.get("cols", 80)),
                rows=int(body.get("rows", 24)),
                accepts_input=bool(body.get("accepts_input", True)))
            return self.send_json(200, {"terminal_id": terminal.id})

        if action == "pair-code":
            return self.send_json(
                200, {"code": self.state.pairing_code(self.state.options.origin)})

        if action == "emit":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            if "data_b64" in body:
                data = base64.b64decode(body["data_b64"])
            else:
                data = str(body.get("text", "")).encode()
            repeat = int(body.get("repeat", 1))
            for _ in range(repeat):
                terminal.append(data)
            return self.send_json(200, {"next_offset": terminal.next_offset})

        if action == "durable":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            offset = int(body.get("offset", terminal.next_offset))
            terminal.durable_offset = min(offset, terminal.next_offset)
            for subscriber in list(terminal.subscribers):
                subscriber.send_text({"type": "durable",
                                      "durable_offset": terminal.durable_offset})
            return self.send_json(200, {"durable_offset": terminal.durable_offset})

        if action == "resize":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            terminal.cols = int(body.get("cols", terminal.cols))
            terminal.rows = int(body.get("rows", terminal.rows))
            for subscriber in list(terminal.subscribers):
                subscriber.send_text({"type": "terminal.resize", "terminal_id": terminal.id,
                                      "cols": terminal.cols, "rows": terminal.rows})
            return self.send_json(200, {"cols": terminal.cols, "rows": terminal.rows})

        if action == "close":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            terminal.state = "closed"
            terminal.close_reason = body.get("reason", "process_exited")
            for subscriber in list(terminal.subscribers):
                subscriber.send_text({
                    "type": "terminal.closed", "terminal_id": terminal.id,
                    "reason": terminal.close_reason,
                    "next_offset": terminal.next_offset,
                    "durable_offset": terminal.durable_offset})
            return self.send_json(200, {"state": "closed"})

        if action == "evict":
            # Force the replay window forward so a resuming client gets a `gap`.
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            count = int(body.get("bytes", 0))
            with terminal.lock:
                count = min(count, len(terminal.buffer))
                del terminal.buffer[:count]
                terminal.earliest_offset += count
            return self.send_json(200, {"earliest_offset": terminal.earliest_offset})

        if action == "drop":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            for subscriber in list(terminal.subscribers):
                subscriber.close(int(body.get("code", 1001)))
            return self.send_json(200, {"dropped": True})

        if action == "pair":
            # Test-only shortcut for the owner-side half of pairing (relay
            # reconciliation §2.2). A real owner runs a register_device challenge on a
            # machine holding the identity key; a device under test has no way to drive
            # that, so this registers the supplied public key as a `client` device
            # directly. It exists only in the fake and skips the signature check.
            try:
                public_key = b64url_decode(str(body.get("public_key", "")))
            except Exception:
                return self.send_error_envelope(400, "invalid_request", "bad public_key")
            if len(public_key) != 32:
                return self.send_error_envelope(422, "validation_failed", "bad key length")

            identity_id = str(body.get("identity_id") or "")
            if not identity_id:
                # Reuse the single test identity so repeated pairings stay together.
                identity_id = next(iter(self.state.identities), None) or fingerprint(
                    "ed25519", b"\x00" * 32)
                self.state.identities.setdefault(identity_id, b"\x00" * 32)

            device_fingerprint = fingerprint("ed25519", public_key)
            for existing in self.state.devices.values():
                if existing["fingerprint"] == device_fingerprint:
                    return self.send_json(200, {"identity_id": existing["identity_id"],
                                                "device_id": existing["device_id"],
                                                "reused": True})
            device_id = str(uuid.uuid4())
            self.state.devices[device_id] = {
                "device_id": device_id,
                "identity_id": identity_id,
                "name": str(body.get("name", "paired test device")),
                "role": "client",
                "fingerprint": device_fingerprint,
                "public_key": public_key,
            }
            return self.send_json(200, {"identity_id": identity_id,
                                        "device_id": device_id, "reused": False})

        if action == "policy":
            self.state.policy.update(body)
            return self.send_json(200, dict(self.state.policy))

        if action == "input":
            terminal_id = parts[2] if len(parts) > 2 else body.get("terminal_id", "")
            terminal = self.state.terminals.get(terminal_id)
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            return self.send_json(200, {
                "frames": [{"sequence": s, "text": d.decode("utf-8", "replace")}
                           for s, d in terminal.received_input],
                "bytes": sum(len(d) for _, d in terminal.received_input),
                "resize_requests": terminal.resize_requests,
            })

        if action == "reset_input":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is not None:
                terminal.received_input = []
            return self.send_json(200, {"ok": True})

        if action == "input_available":
            terminal = self.state.terminals.get(body.get("terminal_id", ""))
            if terminal is None:
                return self.send_error_envelope(404, "not_found", "no such terminal")
            terminal.input_available = bool(body.get("value", True))
            return self.send_json(200, {"input_available": terminal.input_available})

        return self.send_error_envelope(404, "not_found", "unknown test action")

    # -- mirror websocket -------------------------------------------------
    def mirror_upgrade(self, path):
        terminal_id = path.split("/")[3]
        terminal = self.state.terminals.get(terminal_id)

        reject = int(self.state.policy["reject_upgrade_status"])
        if reject:
            return self.send_error_envelope(reject, "unauthorized", "rejected by policy")

        auth = self.authenticated()
        ticket_header = self.headers.get("x-relay-ticket")
        if auth is None and ticket_header:
            record = self.state.tickets.pop(ticket_header, None)
            if record is not None and record["path"] == path and \
                    record["expires_at_ms"] >= now_ms():
                auth = record["auth"]
        if auth is None:
            return self.send_error_envelope(401, "unauthorized", "token required")
        if terminal is None:
            return self.send_error_envelope(404, "not_found", "no such terminal")

        key = self.headers.get("Sec-WebSocket-Key", "")
        if not key or self.headers.get("Sec-WebSocket-Version") != "13":
            return self.send_error_envelope(400, "invalid_request", "bad handshake")
        offered = [p.strip() for p in
                   (self.headers.get("Sec-WebSocket-Protocol") or "").split(",")]
        if MIRROR_V2 in offered and not self.state.policy["offer_v1_only"]:
            version = 2
            selected = MIRROR_V2
        elif MIRROR_V1 in offered:
            version = 1
            selected = MIRROR_V1
        else:
            return self.send_error_envelope(426, "protocol_incompatible", "no common subprotocol")

        accept = base64.b64encode(hashlib.sha1(key.encode() + WS_GUID).digest()).decode()
        response = (
            "HTTP/1.1 101 Switching Protocols\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Accept: {accept}\r\n"
            f"Sec-WebSocket-Protocol: {selected}\r\n\r\n"
        )
        self.wfile.write(response.encode())
        self.wfile.flush()
        self.close_connection = True
        self.serve_mirror(terminal, version)

    def serve_mirror(self, terminal, version):
        connection = MirrorConnection(self, terminal, self.state, version)
        limits = {
            "max_output_frame_bytes": 262144,
            "max_unacked_output_bytes": 0,
            "max_control_message_bytes": 65536,
            "max_active_terminals": 1,
            "replay_capacity_bytes": REPLAY_CAPACITY,
            "heartbeat_interval_seconds": self.state.policy["heartbeat_interval_seconds"],
            "heartbeat_timeout_seconds": self.state.policy["heartbeat_timeout_seconds"],
        }
        if version >= 2:
            limits["max_input_frame_bytes"] = self.state.policy["max_input_frame_bytes"]
        connection.send_text({"type": "ready", "connection_id": str(uuid.uuid4()),
                              "protocol": MIRROR_V2 if version >= 2 else MIRROR_V1,
                              "device_id": None, "limits": limits,
                              "settings_revision": 1})

        try:
            while not connection.closed:
                frame = self.read_ws_frame()
                if frame is None:
                    break
                opcode, payload = frame
                if opcode == 0x8:
                    connection.close(1000)
                    break
                if opcode == 0x9:
                    connection._send_frame(0xA, payload)
                    continue
                if opcode == 0xA:
                    continue
                if opcode == 0x1:
                    self.handle_mirror_text(connection, payload)
                elif opcode == 0x2:
                    self.handle_mirror_binary(connection, payload)
        except (OSError, ConnectionError):
            pass
        finally:
            with terminal.lock:
                if connection in terminal.subscribers:
                    terminal.subscribers.remove(connection)
            connection.closed = True

    def handle_mirror_text(self, connection, payload):
        terminal = connection.terminal
        try:
            message = json.loads(payload.decode())
        except (ValueError, UnicodeDecodeError):
            connection.send_text({"type": "error", "code": "invalid_message",
                                  "message": "not JSON"})
            connection.close(1002)
            return
        kind = message.get("type")

        if kind == "subscribe":
            if connection.subscribed:
                connection.send_text({"type": "error", "code": "already_subscribed",
                                      "message": "one subscribe per connection"})
                connection.close(1002)
                return
            requested = message.get("from_offset")
            if self.state.policy["force_offset_ahead"] or (
                    requested is not None and requested > terminal.next_offset):
                connection.send_text({
                    "type": "error", "code": "offset_ahead",
                    "message": "requested offset is ahead of next_offset",
                    "next_offset": terminal.next_offset,
                    "durable_offset": terminal.durable_offset})
                connection.close(4005)
                return

            start = terminal.earliest_offset if requested is None else int(requested)
            gap = False
            if start < terminal.earliest_offset:
                gap = True
                start = terminal.earliest_offset

            body = {
                "type": "subscribed",
                "terminal_id": terminal.id,
                "requested_from_offset": start if requested is None else int(requested),
                "replay_start_offset": start,
                "next_offset": terminal.next_offset,
                "durable_offset": terminal.durable_offset,
                "earliest_offset": terminal.earliest_offset,
                "terminal_state": terminal.state,
                "label": terminal.label,
                "cols": terminal.cols,
                "rows": terminal.rows,
                "term": terminal.term,
            }
            if connection.version >= 2:
                body["accepts_input"] = terminal.accepts_input
                body["input_available"] = bool(terminal.accepts_input
                                               and terminal.input_available)
            connection.send_text(body)
            if gap:
                connection.send_text({"type": "gap", "terminal_id": terminal.id,
                                      "requested_from_offset": int(requested),
                                      "available_from_offset": terminal.earliest_offset})
            connection.subscribed = True

            # Replay, then register for live delivery with no gap between the two.
            with terminal.lock:
                replay = bytes(terminal.buffer[start - terminal.earliest_offset:])
                terminal.subscribers.append(connection)
            if replay:
                chunk = 65536
                for offset in range(0, len(replay), chunk):
                    connection._send_frame(
                        0x2,
                        b"\x01" + struct.pack("!Q", start + offset) +
                        replay[offset:offset + chunk])
            if terminal.state == "closed":
                connection.send_text({
                    "type": "terminal.closed", "terminal_id": terminal.id,
                    "reason": terminal.close_reason or "process_exited",
                    "next_offset": terminal.next_offset,
                    "durable_offset": terminal.durable_offset})
            return

        if kind == "terminal.resize_request":
            if connection.version < 2 or not terminal.input_available:
                connection.send_text({"type": "error", "code": "input_forbidden",
                                      "message": "no input authority"})
                return
            cols = message.get("cols")
            rows = message.get("rows")
            terminal.resize_requests.append({"cols": cols, "rows": rows})
            if self.state.policy["auto_apply_resize"] and cols and rows:
                # The publisher owns the size; this one accepts and reports it back to
                # every subscriber as an ordinary terminal.resize.
                terminal.cols = int(cols)
                terminal.rows = int(rows)
                for subscriber in list(terminal.subscribers):
                    subscriber.send_text({"type": "terminal.resize",
                                          "terminal_id": terminal.id,
                                          "cols": terminal.cols, "rows": terminal.rows})
            return

        if kind == "pong":
            return

        connection.send_text({"type": "error", "code": "unknown_message_type",
                              "message": "unsupported control message"})
        connection.close(1002)

    def handle_mirror_binary(self, connection, payload):
        terminal = connection.terminal
        if connection.version < 2:
            connection.send_text({"type": "error", "code": "unknown_message_type",
                                  "message": "version 1 has no input frame"})
            connection.close(1002)
            return
        if len(payload) < 9 or payload[0] != 0x02:
            connection.send_text({"type": "error", "code": "invalid_message",
                                  "message": "malformed input frame"})
            connection.close(1002)
            return

        sequence = struct.unpack("!Q", payload[1:9])[0]
        data = payload[9:]
        forced = self.state.policy["input_error"]

        if not data:
            connection.send_text({"type": "error", "code": "invalid_message",
                                  "message": "zero-length input frame"})
            return
        if len(data) > int(self.state.policy["max_input_frame_bytes"]):
            connection.send_text({"type": "error", "code": "limit_exceeded",
                                  "message": "input frame too large"})
            return
        if not terminal.accepts_input:
            connection.send_text({"type": "error", "code": "input_not_accepted",
                                  "message": "terminal did not opt in"})
            return
        if not terminal.input_available:
            connection.send_text({"type": "error", "code": "input_undeliverable",
                                  "message": "no version 2 publisher is connected"})
            return
        if forced:
            connection.send_text({"type": "error", "code": forced,
                                  "message": "input refused by test policy"})
            return
        if sequence != connection.expected_sequence:
            connection.send_text({
                "type": "error", "code": "input_sequence_mismatch",
                "message": f"expected client sequence {connection.expected_sequence}"})
            return

        connection.expected_sequence += 1
        connection.accepted_through = sequence
        connection.relay_sequence += 1
        terminal.received_input.append((sequence, bytes(data)))
        connection.send_text({"type": "input.ack",
                              "accepted_through": connection.accepted_through,
                              "relay_sequence": connection.relay_sequence})

    def read_ws_frame(self):
        header = self.recv_exact(2)
        if header is None:
            return None
        opcode = header[0] & 0x0F
        masked = (header[1] & 0x80) != 0
        length = header[1] & 0x7F
        if length == 126:
            extra = self.recv_exact(2)
            if extra is None:
                return None
            length = struct.unpack("!H", extra)[0]
        elif length == 127:
            extra = self.recv_exact(8)
            if extra is None:
                return None
            length = struct.unpack("!Q", extra)[0]
        if length > 16 * 1024 * 1024:
            return None
        mask = self.recv_exact(4) if masked else b""
        if masked and mask is None:
            return None
        payload = self.recv_exact(length) if length else b""
        if payload is None:
            return None
        if masked:
            payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        return opcode, payload

    def recv_exact(self, count):
        data = b""
        while len(data) < count:
            try:
                chunk = self.connection.recv(count - len(data))
            except OSError:
                return None
            if not chunk:
                return None
            data += chunk
        return data


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address, state):
        self.state = state
        super().__init__(address, Handler)


def main():
    parser = argparse.ArgumentParser(description="Fake Terminal Mirror Relay")
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--origin", default="")
    parser.add_argument("--token-ttl", type=int, default=900)
    parser.add_argument("--tls-cert", default="")
    parser.add_argument("--tls-key", default="")
    parser.add_argument("--verbose", action="store_true")
    options = parser.parse_args()

    state = State(options)
    server = Server((options.host, options.port), state)
    if options.tls_cert:
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(options.tls_cert, options.tls_key)
        server.socket = context.wrap_socket(server.socket, server_side=True)

    port = server.server_address[1]
    scheme = "https" if options.tls_cert else "http"
    if not options.origin:
        options.origin = f"{scheme}://{options.host}:{port}"
    print(f"LISTENING {port}", flush=True)
    # On stderr, deliberately. The integration harness reads one line of stdout and then
    # closes it, so anything written there afterwards kills this process with a broken
    # pipe. A person running the relay by hand still sees this, and can paste it into the
    # app to pair exactly the way a real user pairs.
    print(f"PAIRING-CODE {state.pairing_code(options.origin)}", file=sys.stderr, flush=True)
    try:
        server.serve_forever(poll_interval=0.05)
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
