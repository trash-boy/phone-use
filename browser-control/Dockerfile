# syntax=docker/dockerfile:1
# Reuse the published WDA assets. Both Rust executables are built from this source.
FROM ghcr.io/derrors/iphone-control:0.1.4 AS assets
FROM rust:1.90-trixie AS rust-build
ARG CARGO_BUILD_JOBS=1
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY apps/ ./apps/
COPY vendor/ ./vendor/
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -j "${CARGO_BUILD_JOBS}" -p iphone-control -p ictl-gateway \
    && cp target/release/iphone-control target/release/iphone-control-gateway /usr/local/bin/

FROM rust-build AS gateway-notices
COPY docker/about.toml docker/about.hbs docker/web-about.toml /notices/
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    tool="cargo-about-0.9.3-$(uname -m)-unknown-linux-musl" \
    && curl -fsSL --retry 3 "https://github.com/EmbarkStudios/cargo-about/releases/download/0.9.3/${tool}.tar.gz" -o "/tmp/${tool}.tar.gz" \
    && curl -fsSL --retry 3 "https://github.com/EmbarkStudios/cargo-about/releases/download/0.9.3/${tool}.tar.gz.sha256" -o "/tmp/${tool}.tar.gz.sha256" \
    && (cd /tmp && sha256sum -c "${tool}.tar.gz.sha256") \
    && tar -xzf "/tmp/${tool}.tar.gz" -C /tmp \
    && install -m 755 "/tmp/${tool}/cargo-about" /usr/local/bin/cargo-about \
    && cargo about generate --locked -m crates/ictl-gateway/Cargo.toml -c /notices/about.toml /notices/about.hbs -o /GATEWAY_LICENSES.html \
    && cargo about generate --locked -m crates/ictl-web/Cargo.toml -c /notices/web-about.toml /notices/about.hbs -o /WEB_LICENSES.html

FROM rust-build AS wasm-build
RUN rustup target add wasm32-unknown-unknown
RUN tool="wasm-bindgen-0.2.130-$(uname -m)-unknown-linux-musl" \
    && curl -fsSL --retry 3 "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.130/${tool}.tar.gz" -o "/tmp/${tool}.tar.gz" \
    && curl -fsSL --retry 3 "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.130/${tool}.tar.gz.sha256sum" -o "/tmp/${tool}.tar.gz.sha256sum" \
    && (cd /tmp && awk -v file="${tool}.tar.gz" '{print $1 "  " file}' "${tool}.tar.gz.sha256sum" | sha256sum -c -) \
    && tar -xzf "/tmp/${tool}.tar.gz" -C /tmp \
    && install -m 755 "/tmp/${tool}/wasm-bindgen" /usr/local/bin/wasm-bindgen
COPY browser-control/scripts/build-wasm.sh browser-control/scripts/build-wasm.sh
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target \
    # Source archive timestamps can predate an overlapping build's cached WASM.
    # Recompile the web crate against this stage's source snapshot.
    touch crates/ictl-web/src/lib.rs \
    && WASM_BUILD_PROFILE=release sh browser-control/scripts/build-wasm.sh

FROM node:22-trixie-slim AS web-build
ARG WEB_BASE=/
WORKDIR /web
COPY browser-control/package.json browser-control/package-lock.json ./
COPY browser-control/scripts/patch-device-install.mjs ./scripts/patch-device-install.mjs
RUN npm ci
COPY browser-control/ ./
COPY --from=wasm-build /src/browser-control/pkg/ ./pkg/
RUN npm run build:assets -- --base="${WEB_BASE}"

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 libstdc++6 libssl3t64 zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 control
WORKDIR /app
COPY --from=rust-build /usr/local/bin/iphone-control /usr/local/bin/iphone-control-gateway /usr/local/bin/
COPY --from=assets /usr/share/iphone-control/ /usr/share/iphone-control/
COPY --from=assets /usr/share/licenses/ /usr/share/licenses/
COPY --from=assets /usr/share/doc/iphone-control/ /usr/share/doc/iphone-control/
COPY --from=gateway-notices /GATEWAY_LICENSES.html /usr/share/doc/iphone-control/GATEWAY_LICENSES.html
COPY --from=web-build /web/dist/ ./dist/
COPY --from=gateway-notices /WEB_LICENSES.html ./dist/licenses/rust-wasm.html
COPY vendor/jktcp/LICENSE /usr/share/licenses/jktcp/LICENSE
COPY THIRD_PARTY.md /usr/share/doc/iphone-control/THIRD_PARTY.md
COPY LICENSE ./LICENSE
COPY browser-control/THIRD_PARTY.md ./THIRD_PARTY.md
RUN mkdir -p /app/.runtime && chown -R control:control /app
USER control
ENV HOST=0.0.0.0 PORT=8787 RUNTIME_DIR=/app/.runtime WEB_DIST_DIR=/app/dist ICTL_BIN=/usr/local/bin/iphone-control MALLOC_ARENA_MAX=2
EXPOSE 8787
CMD ["iphone-control-gateway"]
