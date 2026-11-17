# 首版实现与验证记录

## 100 步任务、图片附件与 MJPEG 更新（2026-11-10）

- 本地实际 Rust 网关配合协议 fixture 的完整浏览器套件为 186/186，0 失败、0 跳过。
- Rust 模块检查为网关 22/22、WDA 73/73、WASM 公共逻辑原生测试 3/3。WASM 编译和 `/iphone/` 前缀的优化构建通过。
- 新增覆盖了 100 步后汇报完成、第 101 步被拦截、图片内容校验与会话隔离、模型接收独立参考图片、文件选择/粘贴/拖入/移除、AI 思考期间持续接收 MJPEG、断线关闭、浏览器原始像素到手机点坐标换算及连续流中途旋转。
- 真机接入前探测通过现有 Chrome WebUSB 转发访问 WDA MJPEG。收到 5 张有效 JPEG，首帧约 222 ms，后续间隔约 0.6–1 秒，单帧约 161 kB。该项是直接流通道探测，网页显示延迟与图片导入结果另行记录。

## 首版历史记录

日期：2026-11-09，Asia/Shanghai。分支：`codex/webusb-cloud-control`。代码基线：`4f1f59d`。

## 已交付的代码

浏览器使用发布版 `@limrun/device-install@0.1.0` 处理 USB 选择、配置重试、usbmux 字节通信和 WebSocket 转发。服务器通过每会话虚拟 `usbmuxd` 复用原有 `iphone-control` 的设备、签名、WDA 和恢复能力。首版接入保持原生源码不变，后续真机登录修复涉及两处小改动，见下文。

网页包含设备连接和准备、任务输入、步骤记录、确认与停止、截图和手动控制。AI 接口在服务器配置；未配置时明确显示状态。用户 Mac 在部署架构中只运行 Chrome，原生程序运行于服务器。当前 localhost 测试把本机作为云端角色，不代表外部云部署完成。

## 实际执行的检查

| 检查 | 结果及范围 |
|---|---|
| Node 自动化测试 | 96/96 通过，0 失败、跳过或取消。包含真实 HTTP/WebSocket/TCP/Unix Socket、原生 CLI fixture、发布 SDK 与明确标注的模拟 USB peer |
| 类型检查（当时为 TypeScript，现为 Rust/WASM `cargo check`） | `npm run typecheck` 通过 |
| 前端构建 | `npm run build` 通过 |
| 原生发布包 | `iphone-control0.1.4` 下载 SHA256 校验及 Mac 二进制签名验证通过 |
| 原生兼容性 | 未修改的发布版 daemon 经虚拟 mux 与模拟 Lockdown peer 通信，识别设备并返回 needs_trust；该项使用模拟设备 |
| 原生修复回归 | 14/14 通过，包含连接复用和管道密码输入的修复前失败、修复后通过验证 |
| 真实接入段 | 真实 iPhone18,3/iOS26.5.2，经 Chrome 发布 SDK → WebSocket → 虚拟 mux → 原生 daemon，实际读到型号和系统版本 |
| Chrome 页面 | 实际读取服务器配置，明确显示 AI 未配置；任务建议能填入文本框，未就绪时控制按钮保持禁用；桌面布局截图已检查 |
| 独立审查 | 网关、AI、网页分阶段审查，发现的问题已补 RED→GREEN 回归并修正；整分支最终复审单独记录 |

实际 USB 检查发现配置6的接口申请可能失败，配置3可用。生产代码直接复用 SDK 的候选配置遍历，不硬编码配置3。真实序列号尾部填充、迟到流数据、重复请求、进程组清理、取消后重连、Chrome fetch 接收者、停止按钮和 USB 拔线处理均有回归覆盖。

## 真机登录修复

真实手机已完成配对并确认开发者模式开启。Apple 登录进入双重认证后，原生请求报连接重置。使用相同 reqwest 版本的无账号网络诊断复现了复用 HTTP/1.1 连接时的重置，关闭空闲连接复用后连续请求均收到 HTTP 响应。

`vendor/plume_core/src/lib.rs` 设置 `pool_max_idle_per_host(0)`，让每次 Apple 请求使用新连接。`apps/iphone-control/src/main.rs` 在管道输入时跳过 `stty`。HTTP 回归使用本地明确模拟的服务器，密码输入回归在读取密码前结束 stdin，均验证修复前失败、修复后通过。相关原生测试 14 项通过，本机服务的原生二进制已替换为从当前源码编译的版本。

实际向导已完成 Apple 登录、设备注册、WDA 签名和安装。原生服务报告 WDA 就绪，签名包有效期为 7 天。接收包适配后，网页收到真实手机截图，从网页执行主屏幕按钮后，截图显示手机主屏幕。网页刷新画面也已实测成功，界面显示「可以控制」。在网页自动刷新运行期间，每 5 秒检查一次原生状态，8 次检查全部为 ready。

## USB 传输修复

原网关会把 65536 字节的 TCP 块直接转给 SDK，封装后 USB 包为 65572 字节，超过 usbmuxd 的 49152 字节发送上限。服务器现在按 49116 字节拆分 StreamData，保留发送顺序和写回调。1 MB 上传回归在修复前检测到超限包，修复后校验每包上限和完整字节顺序通过。[usbmuxd 上游 USB 上限](https://github.com/libimobiledevice/usbmuxd/blob/master/src/usb.h)

发布 SDK 会把一次 USB transferIn 当作完整 usbmux 包。新增真实发布 SDK 集成回归将版本回复拆成多次读取，修复前出现 DataView 越界。浏览器接收适配器按长度头组装完整包，处理拆包、合包、DataView 偏移和空传输，限制包长与缓冲区，保留 SDK 的设备配置、配对和协议处理。该集成回归与接收适配单测通过，网页测试合计 96 项通过。

Rust 网关迁移后，容器 Dockerfile 已改为从当前源码编译两个 Rust 可执行文件，复用上游 WDA 资源。容器构建仍待验证。本机验证使用当前源码构建的原生程序，idevice 编译时关闭调试断言，行为与 release 构建一致。

## 尚未完成的验证

- 真实手机坐标点按、滑动、长按、文字输入和完整自然语言任务。截图、网页主屏幕按钮已在后续真机验证中确认，尚未配置实际模型服务。
- 外部 HTTPS 云服务器、Docker 镜像运行和跨网络时延。本机没有 Docker CLI。
- 窄窗口实际视觉断点。浏览器 viewport API 未改变所观察 Chrome 页面的1642×943尺寸，因此未将该项记为通过；布局代码与相关组件测试保留。

原始手机标识、配对凭据、账号、密码和模型密钥不进入此记录或源码包。运行目录、环境文件和截图数据均在 Git 忽略规则内。后续真实操作应在网页向导完成授权后继续验收。

## Rust 网关迁移

服务端入口迁移到 `crates/ictl-gateway`，启动命令和 Docker 运行阶段切换到 Rust。原 Node 实现曾放在 `tests/reference` 作为测试参照，现已删除，相关断言已移到针对 Rust 网关的契约测试（`tests/rust-gateway-*.test.mjs`）。当时保留了 React/TypeScript 工作台、发布版 WebUSB SDK 和原 Rust 控制引擎；工作台随后已改为 Leptos/WASM（见下文「浏览器 Rust/WASM 重构」），当前不再包含 React/TypeScript 代码。

自动化测试使用独立 Rust 服务进程、Node 原生 CLI fixture、HTTP 模型 fixture 和发布版 SDK，覆盖配对恢复互斥、命令去重及缓存淘汰、USB MTU、准备输入、任务确认与停止、静态文件、配置传递和 HTTP 读取期限。

本机缺少 Docker，Linux 容器镜像与公网 VPS 链路尚待独立验证。

本次已在 `http://127.0.0.1:8787/` 用 Rust 网关替换原 Node 服务，Mac Chrome 重新选择真实 iPhone 后恢复原配对记录和签名目录。页面显示“可以控制”并返回真实截图，点击主屏幕后操作和截图流程结束，控件恢复可用且页面无操作错误。模型仍未配置。该证据覆盖本机 Rust 网关和真实 USB 链路，未覆盖公网时延、Linux 容器和真实模型任务。

迁移完成检查通过。Rust 网关 `cargo test -p ictl-gateway --locked` 为 12/12，通过指定实际网关二进制执行 `npm test` 为 104/104，包含 8 项 Rust 服务跨语言集成测试，0 失败、0 跳过。TypeScript、Vite 构建、Rust fmt 和 Clippy `-D warnings` 通过。独立审查已复核配置继承和 HTTP 读取期限两处修正。


## VPS 部署验证

2026-11-09 在 Linux x86_64 VPS 实际完成 Docker 构建与启动，公网入口为 `https://amazingly.top/iphone/`。复用现有域名、证书和 Nginx，通过标准 HTTPS 443 提供网页、REST 和 WSS。初选 9443 在公网访问超时，已撤销临时端口配置。实际修改的 Nginx 配置已备份，原网站主页与既有服务路径另行检查。

依赖锁文件的内网 npm 地址改为公共 registry，版本和完整性校验值保持原样。Dockerfile 默认单任务编译两个 Rust 程序，支持 `WEB_BASE` 构建参数，许可证生成使用经官方 SHA256 校验的 cargo-about 0.9.3 发布包。编译期间增加的临时交换文件已关闭并移除。

运行镜像为 `iphone-control-webusb:vps-20261109`，镜像清单摘要为 `sha256:7bd0eb644fa147ab82980a4b7d7463f19efdd1b1afb6d024a3d5430789cae7eb`。Rust 网关绑定服务器回环地址 8787，运行目录采用独立持久化挂载，容器配置自动重启、内存与进程限制及日志轮转。访问令牌、Magpie 专用调用密钥均保留在服务器受限环境文件中。

公网独立检查已确认以下结果。

- `/iphone/`、JS、CSS 与 `/api/config` 返回 200，TLS 证书校验通过。
- 缺少控制令牌的创建会话请求返回 401，非法 origin 返回 403。
- 合法控制令牌可创建明确标注的合成设备会话，返回正确的公网 WSS 地址。
- WebSocket 错误令牌返回 401，非法 origin 返回 403，合法握手返回 101，检查后关闭合成会话。
- Chrome 实际打开部署入口，显示工作台与已配置的 `codex/gpt-6.1-sol`。
- 本地重新执行 104 项浏览器/协议测试与 12 项 Rust 测试，全部通过。TypeScript 和 `/iphone/` 前缀的 Vite 构建通过。

AI 沿用当前 Rust TaskManager 的观察与操作循环，通过 VPS 现有 Magpie 模型 API 发起视觉 Chat Completions。独立模型探测使用红蓝测试图片，HTTP 200，耗时 3.82 秒，返回正确的 JSON 完成消息。该项验证的是模型 API，未将其计为真实手机任务验收。

用户选择先完成部署、暂不连接手机，本次未进行 VPS → 浏览器 → 真实 iPhone 的操作验收。原有 Mac localhost 真机证据仍见上文。Apple 登录资料、私钥和本地签名目录未迁移到 VPS，首次云端连接需要按网页向导完成配对及 WDA 准备。后续需验证真实手机截图、手动操作、自然语言任务、重连与公网时延。

## 性能、Android 与模型选择更新（2026-11-10）

北京时间 09:51 已切换 VPS 镜像 `iphone-control-webusb:vps-20261110-models`，地址仍为 `https://amazingly.top/iphone/`。镜像清单摘要为 `sha256:7cdcfa0964c46f137cb3c6cdbf530fb7d3e23c29ad22ec9faa36280534fe8553`，源码归档 SHA256 为 `0d8587b7b6fdb26f3a252a0366d63667d504ee03156362c864076afa3d094b93`。该源码包含 Mac 识别性能优化、Android WebADB/H.264 采图与控制，以及任务模型选择。沿用现有服务器配置及持久化目录，旧镜像和 Compose 备份可用于回滚。

服务端从 Magpie `/models` 读取模型目录；本次公网 `/api/config` 返回的 21 个模型 ID 与 Magpie 完全一致。页面可在「任务模型」选择，任务创建校验所选 ID 并固定在任务记录中，默认模型仍由 `LLM_MODEL` 指定。列表失败有提示并保留默认或缓存目录，模型 key 不发送给浏览器。上游失败会显示 HTTP 状态；独立探测观察到 `antigravity/gemini-3.5-flash` 返回 403，因此列入选择框不代表所有模型的权限、额度、图片能力或健康状态均已通过认证。

新增 Nginx `/device/` 的 WSS 转发，沿用中继路径禁止访问日志的配置；`nginx -t` 和平滑重载通过。切换前，临时容器实际验证模型目录、前端资源及官方 scrcpy server SHA256 后退出。切换后工作台、JS、CSS、模型目录和原网站主页均返回 200，TLS 验证正常。

公网验收使用明确标记的合成 Android peer，经 HTTPS 创建临时会话、WSS `/device/ws` 握手和 RPC 接收测试图片。缺少或错误凭据返回 401，非法 origin 返回 403，未列出的模型返回 400。以下三个所选模型经已部署的真实 TaskManager 和 Magpie 完成视觉任务，均正确返回「左红右蓝」，设备动作均为 0，随后临时会话删除并释放中继：

| 模型 | 单次任务耗时 | 结果 |
|---|---:|---|
| `codex/gpt-6-luna` | 4135 ms | completed |
| `antigravity/gemini-3.8-flash` | 9166 ms | completed |
| `antigravity/claude-sonnet-4-6` | 2117 ms | completed |

这是人工生成测试图片的单次接口验收，不是模型速度排名或公网真机任务基准。真实 Xiaomi 平板的本机截图、点击、长按、滑动、中文输入、返回和横竖屏证据与耗时见 README 的平板实测段落；本次未增加公网真机操控证据。Mac 锁屏使人工 Chrome 页面检查未能执行，界面由 SSR 回归、类型检查、生产构建及实际静态资源请求验证。

本次 137 项浏览器/协议测试和 19 项 Rust 网关单元测试通过，0 失败、0 跳过；TypeScript、Vite `/iphone/` 构建、Clippy `-D warnings` 和 Linux Docker 构建通过。新增回归覆盖模型目录合并缓存、非法模型、跨会话模型隔离、目录重定向与凭据保护、默认模型回退及模型选择框的运行状态。

## 浏览器 Rust/WASM 重构（2026-11-10，分支 `codex/rust-wasm-refactor`）

用户选择方案 A：项目自有的页面、连接/会话状态、HTTP/WebSocket、轮询、任务协调、手势坐标、Android 命令/中继/视频帧管理迁移到 Rust/WASM；第三方 iPhone WebUSB、Tango ADB、scrcpy 组件保留在职责明确的 JS 桥中。React 生产源码与相关构建依赖已移除，页面由 Leptos CSR + wasm-bindgen 挂载。

基线为 `e1e3bb46b85ffff1d859dc4e7ce6c014036df41f`，并已合并当时主分支 `a3715cd` 的新增能力（WDA 复用、输入焦点恢复、结构化模型回复、会话占用提示）。本记录对应分支头 `6f4f6f20fcbaa0cb7406e616ba56c006bc9cf548`。

### 自动化验证

- 完整 Node/协议套件 177/177 通过，0 失败、0 跳过（含真实 Rust 网关二进制 fixture）。日志 `.superpowers/sdd/final-all-tests.log`。
- Rust 原生测试 22/22 通过（网关 19 + 浏览器核心 3），默认线程执行。日志 `.superpowers/sdd/final-native-default.log`。
- WASM 目标 Clippy `-D warnings`、网关全目标 Clippy、rustfmt、`git diff --check` 通过。
- Chrome（157.0.8103.39）根路径与 `/iphone/` 生产构建均通过最终验收：WASM 以 `application/wasm` 返回 200，无页面异常或失败请求；验证了“AI 尚未配置”状态、任务输入 UTF-16 计数（测试😀 为 4/4000）、快捷建议回填、Android 切换、令牌输入启用、1440×1100 与 390×844 布局。证据 `final-root-chrome.json`、`final-subpath-chrome.json`。
- 关键行为回归：断线清理、迟到响应归属、同设备重连后旧截图丢弃、任务停止独立请求、Android 大写 URL 方案、视频绘制失败不发布旧画面、准备日志滚动与刷新期间输入焦点保持。

### 尚未在本地验证

- Docker 镜像构建（本机无 Docker CLI）与新增的 `browser.yml` GitHub CI 工作流。
- 真机（iPhone/Android）USB 操作链；浏览器测试使用明确标注的模拟设备与模型 fixture。
- 性能对比：仅记录优化 WASM 体积约 2.35 MB（gzip 约 772 KB），未做网络或真机分阶段基准。

### 偶发说明

原生进程清理测试 `cleanup_escalates_to_kill_an_uncooperative_process_group` 在默认线程下出现过两次偶发失败，单独运行与串行全量运行均通过，未修改相关代码，根因未定位。串行执行 `-- --test-threads=1` 为稳定通过的完整结果。
