//! One-time selection of the TLS cipher provider.
//!
//! rustls declines to choose between compiled-in providers and panics the first time
//! anything needs TLS unless a process-wide default has been installed. Both the HTTP
//! client and the WebSocket client reach for that default, so it has to exist before
//! either opens a connection.
//!
//! It lives here, in the library, rather than in `main`: a caller that uses this crate
//! without the binary — the end-to-end tests do exactly that — needs it just as much,
//! and a panic in a TLS handshake is a poor way to find that out.

use std::sync::Once;

static INSTALL: Once = Once::new();

pub fn ensure_provider() {
    INSTALL.call_once(|| {
        // An error means another provider is already installed, which is fine: the
        // point is only that *some* default exists.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
