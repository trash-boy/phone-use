# 第三方组件

本项目自己的代码以 MIT 发布。下列组件保留各自的许可证。详细分析见 [docs/licensing.md](docs/licensing.md)。

| 组件 | 版本 / 提交 | 许可证 | 引入方式 | 改动 |
|---|---|---|---|---|
| [idevice](https://github.com/jkcoxson/idevice) | 上游 master `89702099`（0.1.68 之后、未发版） | MIT | `vendor/idevice`，`[patch.crates-io]` | 保留该提交的 cryptexd 修复；增加有界的进程启动 NSError 原始归档解析及 domain/code 诊断；增加可选的 typed PID observer、独立 DVT socket 存活观测，以及严格 Boolean 响应解析。原始和修改后文件哈希、改动见 `vendor/idevice/provenance.json`；不改变启动/重试/传输策略 |
| [jktcp](https://crates.io/crates/jktcp) | 0.1.8（间接） | MIT | `vendor/jktcp`，`[patch.crates-io]` | 基于本仓库 `5bea928` 的 jktcp 子目录，保留部分写入取消安全、ACK 重传计时和并发握手修复；增加共享驱动的读写独立推进、有界输出队列、错误传播及回归用例。来源与改动见 `vendor/jktcp/provenance.json`。CI 运行这些回归用例（`cargo test --manifest-path vendor/jktcp/Cargo.toml`，跳过需要 root 建 TUN 设备的 `local_tcp`、`handle_speed`） |
| [rmcp](https://github.com/modelcontextprotocol/rust-sdk) | 3.8.0 | Apache-2.0（部分历史代码 MIT） | crates.io | 无 |
| [apple-codesign](https://github.com/indygreg/apple-platform-rs) | 0.29.0（取自 Impactor `3rdparty/apple-codesign-0.29.0`） | MPL-2.0 | `vendor/apple-codesign`，`[patch.crates-io]` | Impactor 的 0o755 权限补丁（上游提交 428b42f）；本仓库删掉了上游的 `tests/`、`docs/`、`src/testdata/`（单元测试的证书和密钥样例）、`Cargo.lock`、`Cargo.toml.orig`，Cargo.toml 去掉 `[[test]]` 目标、`zip` 依赖从 2.x 升到 9.4.1（修安全告警；只有公证功能用它写 zip，API 兼容），源码未改（因此 vendor 里的单元测试不能运行，本仓库也不跑它们）。来源与改动见 `vendor/apple-codesign/provenance.json`（上游归档哈希未知） |
| [Impactor](https://github.com/claration/impactor) `plume_core` | v2.7.2（提交 b13d99559febd63e3710dfdd0ef8cdd78d10ae13） | MPL-2.0 | `vendor/plume_core` | `src/lib.rs` 关闭 HTTP 空闲连接复用，避免 Apple 登录期间复用连接被重置，文件内标注本地修改并保留 MPL-2.0。`src/utils/certificate.rs` 在同一账号和运行目录的会话间复用有效签名身份、串行创建证书、迁移旧身份、保留待用私钥并在证书配额用尽时安全失败；`src/developer/qh/certs.rs` 容忍证书列表缺少 status/status_code/expiration_date（Cargo.toml 为此增加 tokio `time`/`macros` 与 `libc`）。Cargo.toml 把 workspace 继承展开成显式版本、修正 omnisette 路径，reqwest 用 `rustls-no-provider`（不编 aws-lc-rs），omnisette 改为 `default-features = false, features = ["remote-anisette", "remote-anisette-v3"]`。来源、导入时与当前文件哈希见 `vendor/plume_core/provenance.json`（上游归档哈希未知） |
| Impactor `plume_store` | 同上 | MIT | `vendor/plume_store` | 源码未改；Cargo.toml 展开 workspace 继承。见 `vendor/plume_store/provenance.json` |
| Impactor `omnisette` | 同上 | MPL-2.0 | `vendor/omnisette` | `src/lib.rs` 加了 `anisette_url_v3()`/`set_anisette_url_v3()`（原来 v3 地址没有 setter，自建 anisette 服务配不进去；改动处有注释，文件仍是 MPL-2.0）；Cargo.toml 展开 workspace 继承；reqwest 用 `rustls-no-provider`；本地 anisette（StoreServicesCore，要苹果 Android 版的 `libstoreservicescore.so` 和没有许可证声明的 git 依赖 `android-loader`）改成可选 feature `local-anisette`，默认 feature 里保留与上游一致，本项目不启用（只用远程 anisette v3）。见 `vendor/omnisette/provenance.json` |
| Impactor `plume_utils` | 同上 | MIT | 未 vendor；`crates/ictl-sign/src/bundle.rs` 改写自其 `bundle.rs` | 增加 `BundleType::XCTest`，修复 `.xctest` 漏签 |
| idevice tools | v0.1.68 | MIT | `crates/ictl-device/src/media.rs`、`hid.rs` 的按键/键码表改写自 `tools/src/coredevice_stream.rs`、`hid.rs` | 去掉日志、改错误类型 |
| [WebDriverAgent](https://github.com/appium/WebDriverAgent) | v16.15.3（Appium 官方构建，sha256 2176793538d9…） | BSD-3-Clause | 镜像构建时下载校验，打成未签名 IPA；计划改为本项目 macOS CI 编译 | 不嵌入苹果框架 |
| [usbmuxd](https://github.com/libimobiledevice/usbmuxd) | Debian 包 | GPL-3.0 | 独立进程，不链接 | 无 |

**不随项目分发**：开发者镜像（DDI/Cryptex）、XCTest.framework 等苹果文件；证书、描述文件、配对记录、Apple ID 会话等用户数据。

因为不启用 omnisette 的默认 feature，AGPL-3.0 的过程宏 `remove-async-await` 和没有许可证声明的 `android-loader`/`sysv64` 都不会被编译。

完整的传递依赖许可证清单由 cargo-about 0.9.3 按 `Cargo.lock` 生成（配置 `docker/about.toml`），随镜像放在 `/usr/share/doc/iphone-control/THIRD_PARTY_LICENSES.html`。vendor 组件的许可证另放在 `/usr/share/licenses/<组件>/`（plume_store 的 MIT 许可证在 `/usr/share/licenses/plume_store/LICENSE`），WDA 的在 `/usr/share/licenses/WebDriverAgent/`。这些只随镜像分发：预编译二进制的压缩包只带 `LICENSE` 和本文件。
