//! 单测用：不接真机的守护进程，WDA 换成 [`ictl_wda::fake::ScriptedWda`]，runner 是一直活着的假 runner。

use std::path::PathBuf;
use std::sync::Arc;

use ictl_wda::fake::{Reply, ScriptedWda};
use ictl_wda::{WdaClient, WdaRunner};
use serde_json::json;

use crate::config::Config;
use crate::core::{Daemon, Inner, WdaLink};

/// 临时数据目录里的守护进程。用完删目录
pub(crate) fn daemon(tag: &str) -> (Arc<Daemon>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("ictl-testkit-{tag}-{}", uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    (Daemon::new(Config::from_env(&dir)), dir)
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

/// 一个没锁屏、在普通 App 前台、能读写等待设置的假 WDA；各测试再按需覆盖路由
pub(crate) fn healthy_wda() -> ScriptedWda {
    let w = ScriptedWda::new();
    w.on("GET", "/wda/locked", Reply::value(json!(false)));
    w.on("GET", "/wda/activeAppInfo", Reply::value(json!({"bundleId": "com.example.app"})));
    w.on("GET", "/appium/settings", Reply::value(json!({"waitForIdleTimeout": 10, "animationCoolOffTimeout": 2})));
    w.on("POST", "/appium/settings", Reply::value(json!(null)));
    w.on("POST", "/actions", Reply::value(json!(null)));
    w.on("GET", "/status", Reply::value(json!({"ready": true})));
    w.on("GET", "/wda/screen", Reply::value(json!({"screenSize": {"width": 430, "height": 932}, "scale": 3})));
    w.on("GET", "/rotation", Reply::value(json!({"x": 0, "y": 0, "z": 0})));
    w
}

/// 装上假 WDA 和假 runner（`wda()` 会把它当成可用的 WDA）
pub(crate) fn attach(g: &mut Inner, fake: &ScriptedWda) {
    g.runner = Some(WdaRunner::fake_alive());
    g.wda = Some(WdaClient::new(WdaLink::Fake(fake.clone())));
    g.wda_generation += 1;
}
