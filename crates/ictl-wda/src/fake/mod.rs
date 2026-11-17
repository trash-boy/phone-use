//! 单测用的假 WDA 传输（[`crate::http::Transport`]），给本 crate、`ictl-unlock`、`ictl-daemon` 的单测共用。
//! 只在 `cfg(test)` 或 feature `test-util` 下编译，正式构建里没有。
//!
//! - [`FakeLockScreen`]：按真机行为模拟锁屏界面（解锁流程用）
//! - [`ScriptedWda`]：按路由预设应答或传输错误、记录收到的请求（守护进程工具层的安全规则用）

mod lock_screen;
mod scripted;

pub use lock_screen::{FakeLockScreen, State as LockScreenState};
pub use scripted::{Reply, ScriptedWda};
