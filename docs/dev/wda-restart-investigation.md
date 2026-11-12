# VPS WDA 重启排查（2026-11-11）

## 版本与日志

排查开始时 `main` 与 `origin/main` 均为 `4391875`；现有 WDA 修复分支
`codex/fix-wda-usb-stream-cleanup` 为 `b659e6a`，PR #7 尚未合并。
VPS 实际运行 `iphone-control-webusb:vps-20261111-usb-window-40dbf96`，
容器 running、restart count 为 0。WDA 重启不能归因为容器反复重启。

本轮通过 SSH 读取 VPS 的 `daemon.log` 和 `daemon.stderr.log`，下列时间统一为北京时间：

- 09:07:51、09:08:31、09:08:41：WDA `/status` 超时，第三次失败后 supervisor 重启 runner。
- 09:20:36.357：`hp=27382` 的 ACK 连续推进，每收到一个推进 ACK，紧接着便出现一次 48 字节重传；多次重传集中在不到 1 毫秒内。
- 09:20:49.639：同一连接五次重传后超时，随后 `testmanagerd-main` 报 `Socket(TimedOut)`，XCTest 会话断开。

前一轮关闭 MJPEG 的对照也发生了掉线。日志支持“传输层存在重传计时缺陷”，
尚不足以断言它是全部健康检查超时和 WebSocket 退出的唯一原因。
这里仅保留时间、端口、长度和错误分类，不提交原始用户日志。

## 修复

旧实现给每个 TCP 报文分别保存发送时间、重试次数。某包确认后，下一包即使刚被
累计 ACK 暴露为队首，也按其旧发送时间立即重传。批量 ACK 会连续触发这一行为。
初始 200 毫秒 RTO 也会在浏览器转发到 VPS 的延迟下过早重传。

改为连接级重传计时器：初始 RTO 为 1 秒，超时后翻倍，上限 64 秒；
有效推进 ACK 重新计时并恢复无进展重试预算，全部确认后停表。
重复、过期和超前 ACK 不延长重试寿命；部分 ACK 只保留未确认的后缀，兼容序号回绕。
计时与退避在可取消 I/O 前提交，保持已有发送取消修复。

计时器规则参照 [RFC 6298 §2.1、§5](https://www.rfc-editor.org/rfc/rfc6298.html#section-5)。
当前简化栈没有 RTT 估计器，推进 ACK 后保留已退避的 RTO，避免每换一个队首就重新
采用激进超时；这不是完整 TCP RTT 估计算法。连接持续不确认仍会在有限重试后关闭，
多次超时后的断线检测可能比原来更慢。

## 验证

- 新增的六个 WAN 用例在旧生产逻辑下实际运行，六个全部失败；应用修复后通过。
- 另加批量 48 字节 ACK 用例，模拟 VPS 中每次收包立即 flush 的调用顺序：推进期间无重传突发，停止推进后仍重传最后一包，全部确认后停表。
- `cargo test --locked --manifest-path vendor/jktcp/Cargo.toml --lib -- --skip tests::local_tcp --skip tests::handle_speed`：22 项通过；两个上游 TUN 手工测试需要特权与设备，明确过滤。
- `cargo test --locked -p ictl-wda -p ictl-daemon -p ictl-gateway -p ictl-device --lib`：167 项通过。
- 上述四个项目包 `cargo clippy --locked ... --all-targets -- -D warnings` 通过。

独立 vendored 库的严格 Clippy 发现三个已有告警：`drain_collect`、`type_complexity`、
`chunks_exact_to_as_chunks`，位置均在本次未改动的上游逻辑。没有将它们作为本次重传修复修改。

本轮 Mac USB 列表没有识别到 iPhone，没有部署本次候选代码，也没有完成真机持续运行验收。
部署后需复测预览开启/关闭、Home、截图、观察和掉线恢复，并核对 native 日志及网关关闭原因。
单元测试通过不等于 VPS 真机重启问题已经全部消除。
