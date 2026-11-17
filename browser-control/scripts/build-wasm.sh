#!/bin/sh
set -eu
cd "$(dirname "$0")/../.."
profile=${WASM_BUILD_PROFILE:-debug}
case "$profile" in
    debug) cargo_profile=dev; out_dir=debug; bindgen_flags= ;;
    # The production bundle uses its own Cargo profile (panic=abort, opt-level=s,
    # fat LTO); see [profile.wasm-release] in the workspace Cargo.toml.
    # The function-name section is only for debugging and is most of the file.
    release) cargo_profile=wasm-release; out_dir=wasm-release; bindgen_flags="--remove-name-section --remove-producers-section" ;;
    *) echo "WASM_BUILD_PROFILE must be debug or release" >&2; exit 2 ;;
esac
# The toolchain comes from rust-toolchain.toml (override with RUSTUP_TOOLCHAIN).
# Cargo registry configuration belongs to the developer/CI environment; extra
# cargo arguments (for example --config) can be passed via WASM_CARGO_ARGS.
# shellcheck disable=SC2086
cargo build --locked -p ictl-web --target wasm32-unknown-unknown --profile "$cargo_profile" ${WASM_CARGO_ARGS:-}
# CARGO_TARGET_DIR lets the Docker build keep the WASM cache apart from native builds.
target_dir=${CARGO_TARGET_DIR:-target}
bindgen=${WASM_BINDGEN_BIN:-wasm-bindgen}
wasm="$target_dir/wasm32-unknown-unknown/$out_dir/ictl_web.wasm"
# shellcheck disable=SC2086
"$bindgen" "$wasm" $bindgen_flags --target web --out-dir browser-control/pkg
"$bindgen" "$wasm" --target nodejs --out-dir browser-control/pkg-node
mv browser-control/pkg-node/ictl_web.js browser-control/pkg-node/ictl_web.cjs
printf '{"type":"commonjs"}\n' > browser-control/pkg-node/package.json
if [ "$profile" = release ] && [ "${WASM_OPT:-1}" != 0 ]; then
    # Size-optimise the shipped browser bundle when Binaryen is installed. The
    # unoptimised output is functionally identical, so a missing tool only warns.
    # -Oz shrinks the raw file ~14% but the gzip-compressed size grows a few
    # percent; set WASM_OPT=0 to skip it when only transfer size matters.
    wasm_opt=${WASM_OPT_BIN:-wasm-opt}
    if command -v "$wasm_opt" >/dev/null 2>&1; then
        before=$(wc -c < browser-control/pkg/ictl_web_bg.wasm)
        # Rust >= 1.82 emits bulk-memory/nontrapping-fptoint etc. by default; keep them enabled.
        "$wasm_opt" -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
            --enable-mutable-globals --enable-reference-types --enable-multivalue \
            browser-control/pkg/ictl_web_bg.wasm -o browser-control/pkg/ictl_web_bg.wasm
        after=$(wc -c < browser-control/pkg/ictl_web_bg.wasm)
        echo "wasm-opt -Oz: $before -> $after bytes" >&2
    else
        echo "warning: wasm-opt not found (set WASM_OPT_BIN or install binaryen); shipping WASM without wasm-opt -Oz" >&2
    fi
fi
