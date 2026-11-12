# Browser/cloud third-party components

Project additions are covered by the repository [MIT license](../LICENSE). Existing native components, MPL-2.0 source obligations, Appium BSD-3-Clause WDA and native notices remain described in the original [THIRD_PARTY.md](../THIRD_PARTY.md) and [licensing guide](../docs/licensing.md). The Docker image builds both Rust executables from this source and copies the upstream image's `/usr/share/licenses` and `/usr/share/doc/iphone-control`, including `THIRD_PARTY_LICENSES.html`, plus its unsigned WDA resource. Gateway dependency license texts are generated from the locked Rust manifest as `GATEWAY_LICENSES.html`. Browser Rust/WASM dependency notices use the wasm32 target and are served as `licenses/rust-wasm.html` in the container build.

| Component | Pinned version/source | License/reuse |
|---|---|---|
| [@limrun/device-install](https://www.npmjs.com/package/@limrun/device-install/v/0.1.0) | 0.1.0; installed package metadata | MIT as declared by this published package; imported USB chooser, pairing, relay and IndexedDB store APIs. The browser ESM build receives the local stream lifecycle patch described below. |
| [iphone-control PR #34](https://github.com/Derrors/iphone-control/pull/34) | commit [ff78e757](https://github.com/Derrors/iphone-control/commit/ff78e757) | MIT; `web/interaction.mjs` adapts object-fit screenshot coordinate conversion and pointer tap/swipe/long-press behavior. Copyright (c) 2026 iphone-control contributors. |
| Leptos | 0.8.23 | MIT; Rust/WASM client-rendered UI. Rust dependency metadata is pinned in the root Cargo.lock. |
| wasm-bindgen / web-sys | 0.2.130 / 0.3.107 | MIT OR Apache-2.0; generated browser bindings and DOM/browser API access. |
| ws | 8.22.2 | MIT; test-only dependency, with its license retained in node_modules. |
| plist | 5.1.0 | MIT; test-only dependency, with its own and transitive notices retained in node_modules. |
| Vite / Playwright | package-lock.json | Build/test dependencies retain their package licenses; the browser runtime image contains compiled assets. |

The published device-install package declares MIT and omits a LICENSE file in its npm tarball. The current [typescript-sdk repository root license](https://github.com/limrun-inc/typescript-sdk/blob/main/LICENSE) is Apache-2.0 (Copyright 2026 Limrun). This report distinguishes published artifact metadata from the repository's license; it does not describe the entire upstream repository as MIT. Upstream licensing clarification should accompany redistribution beyond this source implementation.

`scripts/patch-device-install.mjs` modifies only the pinned browser ESM chunk at install/build time. It validates the original SHA-256, releases local streams with RST/ACK, handles peer FIN/RST, wakes pending readers and cancels pending connection opens. The v2 patch also buffers fragmented usbmux packets, retains surplus packets and validates packet/TCP header lengths with a 64 KiB receive limit; it authenticates and upgrades the prior v1 patch. Re-running verifies and reapplies the exact same patch. A changed package version or chunk fails the build and requires review. CommonJS consumers are outside this browser-only patch. The upstream package is downloaded by npm rather than vendored here; its existing copyright and license notices remain applicable.

MIT permission text (published package metadata and adapted MIT interaction code):

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.
