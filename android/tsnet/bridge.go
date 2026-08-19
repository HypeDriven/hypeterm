// Package main exposes an embedded Tailscale node through a small C API.
//
// The node runs entirely in user space (WireGuard over a gVisor netstack), so it needs
// no VpnService, no root and no device-wide routing: only this app's traffic enters the
// tailnet, and only when it asks.
//
// The seam is descriptor handoff. Dial returns a connected AF_UNIX socket whose peer is
// pumped to and from a tailnet connection, so the C++ client keeps using ordinary
// read/write/poll and never learns that the bytes travel through WireGuard. The
// alternative — a loopback listener — was rejected: on Android any app may connect to
// another app's 127.0.0.1 listener, which would turn this into an open proxy into the
// user's tailnet.
//
// Nothing here logs payload, auth keys or node keys (spec §9.3, §12, §15).
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"golang.org/x/sys/unix"
	"tailscale.com/envknob"
	"tailscale.com/ipn"
	"tailscale.com/tsnet"
)

func init() {
	// tsnet otherwise uploads node diagnostics — peer names, addresses, link state —
	// to Tailscale's log service. Nothing about a terminal mirror justifies shipping
	// that off the device (spec §9.3, §12, §15), so opt out before anything starts.
	// This also tells the coordination server that support logs are unavailable,
	// which is the honest thing to report.
	envknob.SetNoLogsNoSupport()
}

// Bounded so a misbehaving peer cannot make the app hold unbounded memory per
// connection (spec §7.4). One buffer per direction per connection.
const pumpBufferBytes = 32 * 1024

// A cap on simultaneous tunnelled connections. The client opens at most a handful
// (auth, discovery, mirror), so this only bounds a runaway.
const maxConnections = 64

type node struct {
	server *tsnet.Server
	cancel context.CancelFunc
}

// How long the server->client direction may keep draining after the client has closed
// its end. The client never half-closes, so its EOF means the connection is finished;
// the grace only lets data already in flight arrive.
const shutdownGrace = 5 * time.Second

var (
	// startMu serialises start/stop so a second start cannot build a second node while
	// the first is still coming up. It is held across slow work, which is why it is not
	// mu: status must stay answerable throughout.
	startMu sync.Mutex

	mu        sync.Mutex
	current   *node
	lastError string

	// Live tunnelled connections, closed when the node stops so no descriptor keeps
	// a dead node alive.
	connMu sync.Mutex
	conns  = map[io.Closer]struct{}{}
	// Slots taken by dials in flight or connections still running. Counted separately
	// from `conns`, which holds two entries per connection and only once it is up.
	reserved int

	// Written by the status poller, read by hypeterm_tsnet_status.
	statusMu    sync.Mutex
	authURL     string
	backendName string
	selfName    string
	selfAddrs   []string
	peerCount   int
	running     bool
)

func setError(err error) {
	mu.Lock()
	defer mu.Unlock()
	if err == nil {
		lastError = ""
		return
	}
	lastError = err.Error()
}

func setErrorf(format string, args ...any) C.int {
	setError(fmt.Errorf(format, args...))
	return -1
}

// copyOut writes s into a caller-provided buffer. Returns the number of bytes written,
// or the negated required size when the buffer is too small, so the caller can retry.
func copyOut(s string, buf *C.char, length C.int) C.int {
	need := len(s) + 1
	if length <= 0 || buf == nil {
		return C.int(-need)
	}
	if need > int(length) {
		return C.int(-need)
	}
	dst := unsafeSlice(buf, int(length))
	copy(dst, s)
	dst[len(s)] = 0
	return C.int(len(s))
}

//export hypeterm_tsnet_start
//
// Brings the node up. state_dir must be app-private storage: it holds the node key.
// auth_key may be empty, in which case the node reports a login URL through
// hypeterm_tsnet_status and waits for the user to authorise it in a browser.
// control_url may be empty for the public coordination server.
//
// Returns 0 on success. Starting an already-started node is a no-op success.
func hypeterm_tsnet_start(stateDir, hostname, authKey, controlURL *C.char, verbose C.int) C.int {
	// Read every C string before blocking: the caller owns that memory only for the
	// duration of the call.
	dir := C.GoString(stateDir)
	name := C.GoString(hostname)
	key := C.GoString(authKey)
	control := C.GoString(controlURL)

	startMu.Lock()
	defer startMu.Unlock()

	mu.Lock()
	already := current != nil
	mu.Unlock()
	if already {
		return 0
	}

	if dir == "" {
		return setErrorf("a state directory is required")
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return setErrorf("state directory: %w", err)
	}
	// The node key lives here; keep it out of any world- or group-readable mode even
	// if the directory already existed with looser permissions.
	if err := os.Chmod(dir, 0o700); err != nil {
		return setErrorf("state directory permissions: %w", err)
	}

	// An Android application process has no HOME, no TMPDIR, and "/" for a working
	// directory, so Go's os.UserCacheDir fails and os.MkdirTemp has nowhere to write.
	// Tailscale looks for somewhere to keep its log state along exactly that path and
	// *panics* when it finds nowhere, taking the whole application down.
	//
	// This has to be done from Go: the Go runtime does not see a setenv the host made
	// through libc, so setting these in C++ before dlopen has no effect.
	for _, scratch := range []struct{ key, path string }{
		{"XDG_CACHE_HOME", filepath.Join(dir, "cache")},
		{"TMPDIR", filepath.Join(dir, "tmp")},
	} {
		if err := os.MkdirAll(scratch.path, 0o700); err != nil {
			return setErrorf("%s: %w", scratch.key, err)
		}
		if err := os.Setenv(scratch.key, scratch.path); err != nil {
			return setErrorf("%s: %w", scratch.key, err)
		}
	}

	if name == "" {
		name = "hypeterm"
	}

	server := &tsnet.Server{
		Dir:      filepath.Join(dir, "tsnet"),
		Hostname: name,
		AuthKey:  key,
		// Empty means the public coordination server; a self-hosted control plane
		// (Headscale, tailnet lock deployments) sets its own URL.
		ControlURL: control,
		// Ephemeral nodes disappear from the tailnet when they stop, which is the
		// right default for a phone that attaches occasionally. A persisted node key
		// still makes restarts silent within a session's lifetime.
		Ephemeral: false,
		// tsnet logs verbosely and its lines carry node and peer identifiers, so they
		// are discarded unless the host asks — which only a debug build does. Without
		// them a node that will not come up gives no reason at all.
		Logf:      logfFor(verbose != 0),
		UserLogf:  logfFor(verbose != 0),
		RunWebClient: false,
	}

	ctx, cancel := context.WithCancel(context.Background())
	if err := server.Start(); err != nil {
		cancel()
		server.Close()
		return setErrorf("tailscale: %w", err)
	}

	n := &node{server: server, cancel: cancel}
	mu.Lock()
	current = n
	lastError = ""
	mu.Unlock()

	// tsnet begins the login itself, using the auth key when there is one. Whether
	// this needs to prompt for an interactive login depends on that, which is why it
	// is passed down rather than decided from the backend state alone.
	go watchStatus(ctx, n, key == "")
	return 0
}

// watchStatus keeps the fields hypeterm_tsnet_status reports up to date. Polling rather
// than the IPN bus keeps the surface small: the client only needs "can I dial yet", the
// login URL, and enough identity to show the user which node they are.
func watchStatus(ctx context.Context, n *node, mayLogInInteractively bool) {
	client, err := n.server.LocalClient()
	if err != nil {
		setError(fmt.Errorf("tailscale: %w", err))
		return
	}
	ticker := time.NewTicker(500 * time.Millisecond)
	defer ticker.Stop()
	askedToLogIn := false
	for {
		st, err := client.Status(ctx)
		if err == nil && st != nil {
			// A node with no auth key can sit in NeedsLogin with no URL ever issued,
			// so ask for one — that is what the user is waiting to be shown.
			//
			// Only when there is no auth key. With one, tsnet is already logging in
			// with it, and starting an interactive login on top discards it and
			// leaves the node waiting for a browser that nobody is going to open.
			if mayLogInInteractively && st.BackendState == ipn.NeedsLogin.String() &&
				!askedToLogIn {
				askedToLogIn = true
				if err := client.StartLoginInteractive(ctx); err != nil {
					setError(fmt.Errorf("tailscale login: %w", err))
					askedToLogIn = false
				}
			}
			if st.BackendState == ipn.Running.String() {
				askedToLogIn = false
			}

			statusMu.Lock()
			authURL = st.AuthURL
			backendName = st.BackendState
			running = st.BackendState == ipn.Running.String()
			selfName = ""
			selfAddrs = nil
			if st.Self != nil {
				// Prefer the name the tailnet knows this node by. Android answers
				// os.Hostname() with "localhost", which identifies nothing.
				selfName = strings.TrimSuffix(st.Self.DNSName, ".")
				if selfName == "" {
					selfName = st.Self.HostName
				}
				for _, addr := range st.Self.TailscaleIPs {
					selfAddrs = append(selfAddrs, addr.String())
				}
			}
			peerCount = len(st.Peer)
			statusMu.Unlock()
		}
		// Poll quickly until the node is up, then back off: a settled node changes
		// state rarely and the app is battery-sensitive (spec §14).
		statusMu.Lock()
		up := running
		statusMu.Unlock()
		if up {
			ticker.Reset(5 * time.Second)
		} else {
			ticker.Reset(500 * time.Millisecond)
		}
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

//export hypeterm_tsnet_status
//
// Writes a JSON status document into buf. Returns the number of bytes written, or the
// negated required size when buf is too small. The document never contains the auth
// key, the node key or any other secret.
func hypeterm_tsnet_status(buf *C.char, length C.int) C.int {
	mu.Lock()
	started := current != nil
	mu.Unlock()

	statusMu.Lock()
	doc := struct {
		Started   bool     `json:"started"`
		Running   bool     `json:"running"`
		Backend   string   `json:"backend_state"`
		AuthURL   string   `json:"auth_url"`
		Hostname  string   `json:"hostname"`
		Addresses []string `json:"addresses"`
		Peers     int      `json:"peers"`
		// Reported so the client can assert the opt-out in init() took effect rather
		// than trusting it.
		NoLogUpload bool `json:"no_log_upload"`
		// Where the Go runtime believes it may write. An Android application process
		// supplies neither by default, and Tailscale panics rather than continue
		// without them, so these are worth being able to see.
		CacheDir string `json:"cache_dir"`
		TempDir  string `json:"temp_dir"`
	}{
		Started:     started,
		Running:     started && running,
		Backend:     backendName,
		AuthURL:     authURL,
		Hostname:    selfName,
		Addresses:   append([]string(nil), selfAddrs...),
		Peers:       peerCount,
		NoLogUpload: envknob.NoLogsNoSupport(),
		CacheDir:    cacheDir(),
		TempDir:     os.TempDir(),
	}
	statusMu.Unlock()

	if !started {
		doc.Backend = "Stopped"
		doc.AuthURL = ""
	}
	encoded, err := json.Marshal(doc)
	if err != nil {
		setError(err)
		return -1
	}
	return copyOut(string(encoded), buf, length)
}

//export hypeterm_tsnet_dial
//
// Dials host:port inside the tailnet and returns a connected descriptor the caller
// owns and must close. Returns -1 on failure; call hypeterm_tsnet_last_error.
//
// host is resolved by the tailnet, not by the device, so MagicDNS names work and a name
// the device cannot resolve is not an error here.
func hypeterm_tsnet_dial(host *C.char, port C.int, timeoutMs C.int) C.int {
	mu.Lock()
	n := current
	mu.Unlock()
	if n == nil {
		return setErrorf("the tailscale node is not running")
	}
	statusMu.Lock()
	up := running
	statusMu.Unlock()
	if !up {
		return setErrorf("the tailscale node is not connected yet")
	}
	if port <= 0 || port > 65535 {
		return setErrorf("port out of range")
	}

	// Take the slot before dialling, not after. Checking and then inserting later
	// leaves a window in which any number of concurrent dials all see room.
	if !reserveConnection() {
		return setErrorf("too many tunnelled connections are open")
	}

	// Every path from here that does not reach a running pump must give the slot back.
	dialed := false
	defer func() {
		if !dialed {
			releaseConnection()
		}
	}()

	timeout := time.Duration(timeoutMs) * time.Millisecond
	if timeoutMs <= 0 {
		timeout = 15 * time.Second
	}
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	address := net.JoinHostPort(C.GoString(host), strconv.Itoa(int(port)))
	remote, err := n.server.Dial(ctx, "tcp", address)
	if err != nil {
		return setErrorf("tailscale dial: %w", err)
	}

	fds, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	if err != nil {
		remote.Close()
		return setErrorf("socketpair: %w", err)
	}
	// net.FileConn duplicates the descriptor, so this file is closed straight after.
	file := os.NewFile(uintptr(fds[1]), "hypeterm-tunnel")
	local, err := net.FileConn(file)
	file.Close()
	if err != nil {
		unix.Close(fds[0])
		remote.Close()
		return setErrorf("descriptor handoff: %w", err)
	}

	track(local)
	track(remote)
	dialed = true
	go pump(local, remote)
	setError(nil)
	return C.int(fds[0])
}

// reserveConnection takes one of the bounded connection slots, or reports that none
// is free.
func reserveConnection() bool {
	connMu.Lock()
	defer connMu.Unlock()
	if reserved >= maxConnections {
		return false
	}
	reserved++
	return true
}

func releaseConnection() {
	connMu.Lock()
	defer connMu.Unlock()
	if reserved > 0 {
		reserved--
	}
}

func track(c io.Closer) {
	connMu.Lock()
	conns[c] = struct{}{}
	connMu.Unlock()
}

func untrack(c io.Closer) {
	connMu.Lock()
	delete(conns, c)
	connMu.Unlock()
}

// pump moves bytes both ways.
//
// The two directions are not symmetric. When the server stops writing, the client is
// told by a half-close so it can still finish its own request. When the *client* stops,
// the connection is over: the client never half-closes its descriptor, so EOF from it
// means it closed, and nothing is left to deliver. The grace period only drains what
// the server already sent, and bounds the goroutine either way.
func pump(local, remote net.Conn) {
	defer func() {
		local.Close()
		remote.Close()
		untrack(local)
		untrack(remote)
		releaseConnection()
	}()

	drained := make(chan struct{})
	go func() {
		defer close(drained)
		copyStream(local, remote)
	}()

	copyStream(remote, local)
	select {
	case <-drained:
	case <-time.After(shutdownGrace):
	}
}

type closeWriter interface{ CloseWrite() error }

func copyStream(dst, src net.Conn) {
	buffer := make([]byte, pumpBufferBytes)
	// Errors are the normal way a connection ends; there is nothing to report and
	// nothing that may be logged, since the bytes are terminal traffic.
	_, _ = io.CopyBuffer(writerOnly{dst}, readerOnly{src}, buffer)
	if cw, ok := dst.(closeWriter); ok {
		_ = cw.CloseWrite()
	} else {
		_ = dst.Close()
	}
}

// io.CopyBuffer ignores the supplied buffer when either side implements ReadFrom or
// WriteTo; these wrappers hide those so the copy stays inside the bound above.
type writerOnly struct{ io.Writer }
type readerOnly struct{ io.Reader }

//export hypeterm_tsnet_stop
//
// Stops the node and closes every tunnelled connection. Descriptors already handed to
// the caller stay valid but report end-of-stream.
func hypeterm_tsnet_stop() {
	startMu.Lock()
	defer startMu.Unlock()

	mu.Lock()
	n := current
	current = nil
	mu.Unlock()
	if n == nil {
		return
	}

	connMu.Lock()
	open := make([]io.Closer, 0, len(conns))
	for c := range conns {
		open = append(open, c)
	}
	conns = map[io.Closer]struct{}{}
	connMu.Unlock()
	for _, c := range open {
		_ = c.Close()
	}

	n.cancel()
	_ = n.server.Close()

	statusMu.Lock()
	running = false
	authURL = ""
	backendName = "Stopped"
	selfName = ""
	selfAddrs = nil
	peerCount = 0
	statusMu.Unlock()
}

//export hypeterm_tsnet_logout
//
// Forgets the node key so the next start requires a fresh authorisation. Used when the
// user removes the tailnet from the app.
func hypeterm_tsnet_logout(stateDir *C.char) C.int {
	mu.Lock()
	n := current
	mu.Unlock()
	if n != nil {
		client, err := n.server.LocalClient()
		if err == nil {
			ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
			err = client.Logout(ctx)
			cancel()
			if err != nil && !errors.Is(err, context.DeadlineExceeded) {
				setError(fmt.Errorf("tailscale logout: %w", err))
			}
		}
	}
	hypeterm_tsnet_stop()

	dir := C.GoString(stateDir)
	if dir == "" {
		return 0
	}
	if err := os.RemoveAll(filepath.Join(dir, "tsnet")); err != nil {
		return setErrorf("clearing tailscale state: %w", err)
	}
	return 0
}

//export hypeterm_tsnet_last_error
//
// Writes the most recent failure into buf. Same return convention as status.
func hypeterm_tsnet_last_error(buf *C.char, length C.int) C.int {
	mu.Lock()
	message := lastError
	mu.Unlock()
	return copyOut(message, buf, length)
}

// logfFor routes tsnet's own logging to stderr, which the Android host relays to the
// system log in debug builds, or discards it.
func logfFor(verbose bool) func(string, ...any) {
	if !verbose {
		return discardLogf
	}
	return func(format string, args ...any) {
		fmt.Fprintf(os.Stderr, "tsnet: "+format+"\n", args...)
	}
}

func cacheDir() string {
	dir, err := os.UserCacheDir()
	if err != nil {
		return "unavailable: " + err.Error()
	}
	return dir
}

func discardLogf(format string, args ...any) {}

func main() {}
