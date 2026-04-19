# ─── Stage 1: builder ────────────────────────────────────────────────────────
FROM rust:1.87-bookworm AS builder

WORKDIR /build

# Cache dependency compilation by copying manifests first.
COPY Cargo.toml Cargo.lock ./
COPY crates/core/Cargo.toml          crates/core/Cargo.toml
COPY crates/api/Cargo.toml           crates/api/Cargo.toml
COPY crates/adapters/Cargo.toml      crates/adapters/Cargo.toml
COPY crates/adapter-open-meteo/Cargo.toml   crates/adapter-open-meteo/Cargo.toml
COPY crates/adapter-elgato-lights/Cargo.toml crates/adapter-elgato-lights/Cargo.toml
COPY crates/adapter-roku-tv/Cargo.toml       crates/adapter-roku-tv/Cargo.toml
COPY crates/adapter-ollama/Cargo.toml        crates/adapter-ollama/Cargo.toml
COPY crates/adapter-zigbee2mqtt/Cargo.toml   crates/adapter-zigbee2mqtt/Cargo.toml
COPY crates/scenes/Cargo.toml        crates/scenes/Cargo.toml
COPY crates/automations/Cargo.toml   crates/automations/Cargo.toml
COPY crates/lua-host/Cargo.toml      crates/lua-host/Cargo.toml
COPY crates/store-sql/Cargo.toml     crates/store-sql/Cargo.toml

# Stub out every lib/main so `cargo build` can compile deps without real source.
RUN for crate in core lua-host adapters scenes automations store-sql \
        adapter-open-meteo adapter-elgato-lights adapter-roku-tv \
        adapter-ollama adapter-zigbee2mqtt; do \
      mkdir -p crates/$crate/src && echo "// stub" > crates/$crate/src/lib.rs; \
    done && \
    mkdir -p crates/api/src && echo "fn main() {}" > crates/api/src/main.rs && \
    cargo build --release -p api 2>/dev/null || true && \
    # Remove the stub artifacts so the real build runs in full.
    find target/release/.fingerprint -name "*api*" -delete 2>/dev/null || true

# Copy real source and build the release binary.
COPY crates/ crates/
RUN cargo build --release -p api

# ─── Stage 2: runtime ────────────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates is needed for HTTPS adapter calls (open-meteo, etc.).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Non-root user for the service.
RUN useradd --system --no-create-home --shell /sbin/nologin smart-home

COPY --from=builder /build/target/release/api /usr/local/bin/smart-home

# /config holds the TOML config file and Lua assets (scenes, automations, scripts).
# /data   holds the SQLite database.
# Both are declared as volumes so they can be bind-mounted from the host.
VOLUME ["/config", "/data"]

ENV SMART_HOME_CONFIG=/config/default.toml
ENV SMART_HOME_DATA_DIR=/data

USER smart-home

EXPOSE 3001

ENTRYPOINT ["smart-home"]
