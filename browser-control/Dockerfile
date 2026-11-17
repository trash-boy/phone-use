# syntax=docker/dockerfile:1
# Base images are pinned by multi-arch index digest (tag kept for readability).
# The Rust image matches rust-toolchain.toml; update tag and digest together, e.g.
#   docker buildx imagetools inspect rust:1.99.1-trixie
ARG ASSETS_IMAGE=ghcr.io/derrors/iphone-control:0.1.4@sha256:eacd305229fc825872f386010a86191c2f3172c660835e3d74cff77c77766e7e
ARG RUST_IMAGE=rust:1.99.1-trixie@sha256:8a4e90c7946b74626488b1446f712c0fe0ed1a596cbd3d871684bc9c5433ae07
ARG NODE_IMAGE=node:22-trixie-slim@sha256:68c76cfad89921b927eae28ba21407475e29d0ce450afcdbe70c1532db042b64
ARG PYTHON_IMAGE=python:3.11-slim-trixie@sha256:07d4824036af339fa1b6ef769cc99d9c10a141769538b52b3b597c84f7a7b535
# Reuse the published WDA assets. Both Rust executables are built from this source.
FROM ${ASSETS_IMAGE} AS assets
FROM ${RUST_IMAGE} AS rust-build
# Parallel rustc jobs. 2 keeps peak memory within a small VPS; CI passes $(nproc).
ARG CARGO_BUILD_JOBS=2
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
# The WASM build has its own target cache (separate id and CARGO_TARGET_DIR), so
# it never reuses artifacts written by the native rust-build stage above.
ENV CARGO_TARGET_DIR=/src/target-wasm
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=phone-use-wasm-target,target=/src/target-wasm,sharing=locked \
    WASM_BUILD_PROFILE=release sh browser-control/scripts/build-wasm.sh

FROM ${NODE_IMAGE} AS web-build
ARG WEB_BASE=/
WORKDIR /web
COPY browser-control/package.json browser-control/package-lock.json ./
COPY browser-control/scripts/patch-device-install.mjs ./scripts/patch-device-install.mjs
RUN npm ci
COPY browser-control/ ./
COPY --from=wasm-build /src/browser-control/pkg/ ./pkg/
RUN npm run build:assets -- --base="${WEB_BASE}"

FROM ${PYTHON_IMAGE}
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libgcc-s1 libstdc++6 libssl3t64 zlib1g libgomp1 libgl1 libglib2.0-0t64 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 control
WORKDIR /app
COPY browser-control/ocr/requirements.txt /app/ocr/requirements.txt
RUN python3 -m venv /opt/ocr \
    && /opt/ocr/bin/python3 -m pip install --no-cache-dir -r /app/ocr/requirements.txt
COPY browser-control/ocr/worker.py /app/ocr/worker.py
COPY --from=rust-build /usr/local/bin/iphone-control /usr/local/bin/iphone-control-gateway /usr/local/bin/
COPY --from=assets /usr/share/iphone-control/ /usr/share/iphone-control/
COPY --from=assets /usr/share/licenses/ /usr/share/licenses/
COPY --from=assets /usr/share/doc/iphone-control/ /usr/share/doc/iphone-control/
COPY --from=gateway-notices /GATEWAY_LICENSES.html /usr/share/doc/iphone-control/GATEWAY_LICENSES.html
COPY --from=web-build /web/dist/ ./dist/
COPY --from=gateway-notices /WEB_LICENSES.html ./dist/licenses/rust-wasm.html
COPY LICENSE ./LICENSE
COPY browser-control/THIRD_PARTY.md ./THIRD_PARTY.md
RUN mkdir -p /app/.runtime && chown -R control:control /app
USER control
# Download the two mobile CPU models into a cache carried in this image.
# Runtime requests reuse this cache and the gateway's persistent Python child.
ENV PADDLE_PDX_CACHE_HOME=/home/control/.paddlex PADDLE_PDX_DISABLE_MODEL_SOURCE_CHECK=True
RUN /opt/ocr/bin/python3 /app/ocr/worker.py --prepare
ENV OCR_PYTHON=/opt/ocr/bin/python3 OCR_WORKER=/app/ocr/worker.py
ENV HOST=0.0.0.0 PORT=8787 RUNTIME_DIR=/app/.runtime WEB_DIST_DIR=/app/dist ICTL_BIN=/usr/local/bin/iphone-control MALLOC_ARENA_MAX=2
EXPOSE 8787
CMD ["iphone-control-gateway"]
