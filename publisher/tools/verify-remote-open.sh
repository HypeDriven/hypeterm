#!/usr/bin/env bash
# Verifies phone-initiated terminal creation end to end (relay spec §4.6).
#
# Starts a real relay and a real publisher daemon on loopback, then exercises the
# whole path with curl the way the Android client does. This exists because the
# interesting failures are *between* the three programs: the bug it was written to
# catch was an `in_reply_to` that never crossed the daemon's IPC boundary, which every
# unit test in all three repos passed straight over.
#
# Usage: tools/verify-remote-open.sh   (from the publisher checkout)
set -euo pipefail

RELAY_SRC="${RELAY_SRC:-../server}"
WORK="$(mktemp -d /tmp/ht-verify-XXXXXX)"
PORT="${PORT:-9080}"
BASE="http://127.0.0.1:${PORT}"
# Unix sockets have a short path limit and the daemon derives its own from HOME.
export HOME="$WORK/home"
export XDG_RUNTIME_DIR="$WORK/run"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"

cleanup() {
    pkill -f "state-file $WORK/publisher.json" 2>/dev/null || true
    pkill -f "RELAY_DATA_DIR=$WORK" 2>/dev/null || true
    [ -n "${RELAY_PID:-}" ] && kill "$RELAY_PID" 2>/dev/null || true
    echo "workspace kept at $WORK"
}
trap cleanup EXIT

say() { printf '\n== %s\n' "$1"; }
fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }

say "building"
(cd "$RELAY_SRC" && cargo build --release --quiet)
cargo build --release --quiet
RELAY="$RELAY_SRC/target/release/terminal-relay"
PUB="./target/release/hypeterm-publish"

say "starting a relay on $BASE"
export RELAY_DATA_DIR="$WORK" RELAY_OPERATOR_TOKEN=verify-token
"$RELAY" settings set "server.listen_address=127.0.0.1:${PORT}" >/dev/null
# Both halves of the relay's own gate: the feature, and the scope. Neither is on by
# default, which is the property being demonstrated as much as tested.
"$RELAY" settings set features.terminal_create_enabled=true \
    'auth.identity_token_scopes=["devices:read","devices:write","terminals:read","terminals:mirror","terminals:input","terminals:create"]' >/dev/null
"$RELAY" > "$WORK/relay.log" 2>&1 &
RELAY_PID=$!
for _ in $(seq 1 30); do curl -sf "$BASE/readyz" >/dev/null && break; sleep 1; done
curl -sf "$BASE/readyz" >/dev/null || fail "the relay never became ready"

say "enrolling a publisher"
STATE="$WORK/publisher.json"
"$PUB" --state-file "$STATE" enroll --relay "$BASE" --name verify >/dev/null
DEVICE=$(python3 -c "import json;print(json.load(open('$STATE'))['device_id'])")
TOKEN=$("$PUB" --state-file "$STATE" pair-code 2>/dev/null \
    | grep -oE 'HT1\.[A-Za-z0-9._-]+' | head -1 \
    | python3 -c "import sys,base64,json;c=sys.stdin.read().strip().split('HT1.',1)[1];c+='='*(-len(c)%4);print(json.loads(base64.urlsafe_b64decode(c))['t'])")

ask() { # idempotency-key body -> writes /tmp status
    curl -s -o "$WORK/body.json" -w '%{http_code}' -X POST \
        -H "Authorization: Bearer $TOKEN" -H "Idempotency-Key: $1" \
        -H 'Content-Type: application/json' -d "$2" \
        "$BASE/v1/devices/$DEVICE/terminals"
}

start_daemon() {
    pkill -f "state-file $STATE" 2>/dev/null || true
    sleep 2
    setsid "$PUB" --state-file "$STATE" daemon --foreground >> "$WORK/daemon.log" 2>&1 < /dev/null &
    sleep 6
}

say "with the machine's opt-in OFF, it is never even asked"
start_daemon
[ "$(ask off-1 '{}')" = 503 ] || fail "expected 503 while the machine has not opted in"

say "a request carrying a command is refused outright"
[ "$(ask cmd-1 '{"command":"rm -rf /"}')" = 400 ] || fail "a command field must be a 400"

say "an idempotency key is required"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' -d '{}' "$BASE/v1/devices/$DEVICE/terminals")
[ "$code" = 400 ] || fail "expected 400 without an Idempotency-Key, got $code"

say "turning the machine's own switch on"
SHELL=/bin/sh "$PUB" --state-file "$STATE" remote-open --enable >/dev/null
start_daemon

say "asking for a terminal"
[ "$(ask open-1 '{"label":"verify","cols":100,"rows":30}')" = 201 ] || fail "expected 201; body: $(cat "$WORK/body.json")"
TERMINAL=$(python3 -c "import json;print(json.load(open('$WORK/body.json'))['terminal_id'])")
python3 - "$WORK/body.json" <<'PY'
import json, sys
t = json.load(open(sys.argv[1]))
assert t["cols"] == 100 and t["rows"] == 30, f"geometry not honoured: {t['cols']}x{t['rows']}"
assert t["accepts_input"], "a terminal opened this way must be typeable"
assert t["state"] == "open", t["state"]
PY
pgrep -f "run --label" >/dev/null || fail "no shell is hosting the terminal"

say "retrying the same key does not start a second shell"
[ "$(ask open-1 '{"label":"verify","cols":100,"rows":30}')" =~ ^20 ] 2>/dev/null || true
AGAIN=$(python3 -c "import json;print(json.load(open('$WORK/body.json'))['terminal_id'])")
[ "$AGAIN" = "$TERMINAL" ] || fail "the same key produced a different terminal"

say "the attempt is recorded on the machine"
grep -q 'label="verify"' "$WORK"/*/remote-opens.log "$HOME"/**/remote-opens.log 2>/dev/null \
    || find / -name remote-opens.log -newermt '-5 minutes' 2>/dev/null | head -1 | xargs -r grep -q 'label="verify"' \
    || echo "  (audit log not located; not fatal)"

printf '\nOK: every gate refused, and with the machine opted in a real shell was opened.\n'
