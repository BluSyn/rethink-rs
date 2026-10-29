# Multi-stage Rust build for rethink-cloud.
#
# Faster rebuilds (classic layer cache — works without BuildKit/buildx):
#  1) Copy only Cargo manifests + dummy sources → compile crates.io deps
#  2) That layer stays cached when only app code/html changes
#  3) Copy real crates/html → cargo rebuilds workspace packages only
#
# Optional BuildKit bonus (if buildx works): set RETHINK_DOCKER_CACHE=1 to use
# registry/target cache mounts (see comments at the bottom of the build stage).
#
# time 0.3.x needs rustc >= 1.88; icu_* 2.2 needs >= 1.86.

FROM rust:1.99-bookworm AS build
WORKDIR /app

ENV CARGO_TERM_COLOR=always \
    CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse

# ── Layer A: manifests only (invalidates when deps / crate graph change) ──
COPY Cargo.toml Cargo.lock ./
COPY crates/rethink-util/Cargo.toml crates/rethink-util/
COPY crates/rethink-core/Cargo.toml crates/rethink-core/
COPY crates/rethink-devices/Cargo.toml crates/rethink-devices/
COPY crates/rethink-bridge/Cargo.toml crates/rethink-bridge/
COPY crates/rethink-cloud/Cargo.toml crates/rethink-cloud/
COPY crates/rethink-setup/Cargo.toml crates/rethink-setup/
COPY crates/rethink-tools/Cargo.toml crates/rethink-tools/

# Minimal sources so cargo can resolve path members and compile dependencies
# without the full (frequently changing) source tree.
RUN set -eux; \
    for crate in rethink-util rethink-core rethink-devices rethink-bridge; do \
      mkdir -p "crates/${crate}/src"; \
      printf '%s\n' '#![allow(dead_code)]' 'pub fn _docker_dep_stub() {}' \
        > "crates/${crate}/src/lib.rs"; \
    done; \
    mkdir -p crates/rethink-cloud/src; \
    printf 'fn main() {}\n' > crates/rethink-cloud/src/main.rs; \
    mkdir -p crates/rethink-setup/src; \
    printf 'fn main() {}\n' > crates/rethink-setup/src/main.rs; \
    mkdir -p crates/rethink-tools/src/bin; \
    for bin in packet_parser packet_sender rethink_capture rethink_mcp lgcloud_monitor; do \
      printf 'fn main() {}\n' > "crates/rethink-tools/src/bin/${bin}.rs"; \
    done; \
    mkdir -p html; \
    printf 'stub\n' > html/.keep

# Compile transitive deps + dummy workspace crates into this layer's target/.
# On source-only rebuilds this step is fully cached.
RUN cargo build --release \
      -p rethink-cloud -p rethink-setup -p rethink-tools \
    && rm -f target/release/rethink-cloud \
             target/release/rethink-setup \
             target/release/packet-parser \
             target/release/packet-sender \
             target/release/rethink-capture \
             target/release/rethink-mcp \
             target/release/lgcloud-monitor \
             target/release/deps/rethink_* \
    && rm -rf target/release/.fingerprint/rethink-* \
              target/release/incremental

# ── Layer B: real sources (invalidates on any code / html change) ──────────
COPY crates ./crates
COPY html ./html

# Ensure cargo treats our sources as newer than the dummy stubs.
RUN find crates html -type f \( \
        -name '*.rs' -o -name '*.html' -o -name '*.js' -o -name '*.css' \
        -o -name '*.woff2' -o -name '*.toml' \
      \) -exec touch {} +

# Management UI shows this short SHA (fallback "dev" when unset / no .git).
# Build: docker build --build-arg RETHINK_GIT_SHA=$(git rev-parse --short=7 HEAD) ...
ARG RETHINK_GIT_SHA=dev
ENV RETHINK_GIT_SHA=${RETHINK_GIT_SHA}

# Rebuild only workspace packages; dependency objects remain from layer A.
RUN cargo build --release \
      -p rethink-cloud -p rethink-setup -p rethink-tools \
    && strip target/release/rethink-cloud \
             target/release/rethink-setup \
             target/release/packet-parser \
             target/release/packet-sender \
             target/release/rethink-capture \
             target/release/rethink-mcp \
             target/release/lgcloud-monitor \
    && mkdir -p /out \
    && cp target/release/rethink-cloud \
          target/release/rethink-setup \
          target/release/packet-parser \
          target/release/packet-sender \
          target/release/rethink-capture \
          target/release/rethink-mcp \
          target/release/lgcloud-monitor \
          /out/

FROM debian:bookworm-slim AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -r app \
    && useradd -r -g app app

COPY --from=build /out/rethink-cloud /usr/local/bin/rethink-cloud
COPY --from=build /out/rethink-setup /usr/local/bin/rethink-setup
COPY --from=build /out/packet-parser /usr/local/bin/packet-parser
COPY --from=build /out/packet-sender /usr/local/bin/packet-sender
COPY --from=build /out/rethink-capture /usr/local/bin/rethink-capture
COPY --from=build /out/rethink-mcp /usr/local/bin/rethink-mcp
COPY --from=build /out/lgcloud-monitor /usr/local/bin/lgcloud-monitor
COPY config.jsonc /app/config.json

RUN mkdir -p /app/data && chown -R app:app /app
USER app

EXPOSE 443 8883 1884 46030 47878 44401
CMD ["sh", "-c", "[ -f /app/data/config.json ] || cp /app/config.json /app/data/config.json; exec rethink-cloud /app/data/config.json"]
