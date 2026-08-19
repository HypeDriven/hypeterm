# Terminal Mirror Relay container image (spec §8).
#
# Properties this image is required to have:
#   * runs as a non-root user
#   * minimal, pinned base image
#   * one configurable HTTP port, default 8080
#   * logs to standard output and error
#   * handles SIGTERM, drains, and exits inside the shutdown deadline
#   * a health check that needs no shell
#   * a read-only root filesystem, with only /var/lib/terminal-relay and /tmp writable
#
# Base images are pinned to explicit versions. For a fully reproducible build, append
# an image digest to each tag (`rust:1.95-trixie@sha256:...`).

# --------------------------------------------------------------------- build stage

FROM rust:1.95-trixie AS build

WORKDIR /src

# Warm the dependency cache first: SQLite is compiled from source by the bundled
# rusqlite feature, and it dominates build time.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked --quiet 2>/dev/null || true \
    && rm -rf src

COPY src ./src
COPY spec.md ./spec.md

# COPY preserves the build context's modification times, which can be older than the
# cache-warming build's artifacts. Without refreshing them, cargo considers the real
# sources unchanged and ships the placeholder binary.
RUN find src -type f -exec touch {} + \
    && cargo build --release --locked --bin terminal-relay \
    && strip target/release/terminal-relay \
    && test -n "$(./target/release/terminal-relay healthcheck 127.0.0.1:1 2>&1)" \
        || (echo "build produced a binary that does not respond to healthcheck" && exit 1)

# The data directory is created here so it can be copied in with the right ownership;
# the runtime image has no shell to create it.
RUN mkdir -p /stage/var/lib/terminal-relay

# ------------------------------------------------------------------- runtime stage

FROM gcr.io/distroless/cc-debian12:nonroot AS runtime

# Matches the `nonroot` user in the distroless base.
ARG UID=65532
ARG GID=65532

COPY --from=build /src/target/release/terminal-relay /usr/local/bin/terminal-relay
COPY --from=build --chown=${UID}:${GID} /stage/var/lib/terminal-relay /var/lib/terminal-relay

USER ${UID}:${GID}

# Durable state must live on a mounted volume, never the container's writable layer.
VOLUME ["/var/lib/terminal-relay"]

# The default listen address is a database setting (server.listen_address); this
# EXPOSE documents the default and does not constrain it.
EXPOSE 8080

ENV RELAY_DATA_DIR=/var/lib/terminal-relay

# Uses the binary's own probe, since the image has no shell or curl. Point it at the
# isolated health listener when one is configured, so the check keeps working if the
# main listener moves to TLS.
HEALTHCHECK --interval=15s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/terminal-relay", "healthcheck", "127.0.0.1:8080", "/healthz"]

ENTRYPOINT ["/usr/local/bin/terminal-relay"]
