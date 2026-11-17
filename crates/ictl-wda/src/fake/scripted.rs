//! 按路由预设应答的假 WDA。
//!
//! 匹配规则：请求路径先去掉 `/session/<id>` 前缀，再按「方法 + 路径前缀」找规则；一次性规则（[`ScriptedWda::once`]）
//! 先于常驻规则（[`ScriptedWda::on`]），同类里后加的优先。都没对上时：`POST /session` 给会话 `S`，其余报 404。
//! 传输错误按 [`crate::http::Transport`] 的约定分类：`NotConnected` 是没送到（[`crate::WdaError::Unavailable`]），
//! `TimedOut` 是发出后超时，[`crate::http::AFTER_SENT`] 是发出后断开

use std::io::ErrorKind;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::http::Transport;

/// 一条预设的应答
#[derive(Debug, Clone)]
pub enum Reply {
    /// 状态码 + JSON 正文
    Json(u16, Value),
    /// 传输错误（见模块说明）
    Fail(ErrorKind),
    /// 一直不应答（调用方的时限到了才返回）
    Hang,
}

impl Reply {
    /// `{"value": v}`，状态 200
    pub fn value(v: Value) -> Self {
        Self::Json(200, json!({ "value": v }))
    }
    pub fn unavailable() -> Self {
        Self::Fail(ErrorKind::NotConnected)
    }
    pub fn timeout() -> Self {
        Self::Fail(ErrorKind::TimedOut)
    }
    pub fn interrupted() -> Self {
        Self::Fail(crate::http::AFTER_SENT)
    }
}

struct Rule {
    method: String,
    prefix: String,
    reply: Reply,
    once: bool,
}

#[derive(Default)]
struct Script {
    rules: Vec<Rule>,
    seen: Vec<(String, String)>,
    /// MJPEG 单帧：None 是不支持（走普通截图），Some(Err) 是读帧失败
    jpeg: Option<Result<Vec<u8>, ErrorKind>>,
    jpeg_reads: usize,
}

#[derive(Clone, Default)]
pub struct ScriptedWda(Arc<Mutex<Script>>);

impl ScriptedWda {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, method: &str, prefix: &str, reply: Reply, once: bool) -> &Self {
        self.0.lock().unwrap().rules.push(Rule { method: method.into(), prefix: prefix.into(), reply, once });
        self
    }

    /// 常驻规则：每次匹配都给这个应答
    pub fn on(&self, method: &str, prefix: &str, reply: Reply) -> &Self {
        self.add(method, prefix, reply, false)
    }

    /// 一次性规则：用掉一次就删掉
    pub fn once(&self, method: &str, prefix: &str, reply: Reply) -> &Self {
        self.add(method, prefix, reply, true)
    }

    /// MJPEG 单帧的结果（见 [`Transport::screenshot_jpeg`]）
    pub fn set_jpeg(&self, frame: Option<Result<Vec<u8>, ErrorKind>>) {
        self.0.lock().unwrap().jpeg = frame;
    }

    /// 收到的请求（方法, 去掉会话前缀的路径）
    pub fn requests(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().seen.clone()
    }

    /// 收到过几次 `method` 且路径以 `prefix` 开头的请求
    pub fn count(&self, method: &str, prefix: &str) -> usize {
        self.0.lock().unwrap().seen.iter().filter(|(m, p)| m == method && p.starts_with(prefix)).count()
    }

    pub fn jpeg_reads(&self) -> usize {
        self.0.lock().unwrap().jpeg_reads
    }

    fn reply(&self, method: &str, path: &str) -> Reply {
        let mut s = self.0.lock().unwrap();
        let path = strip_session(path);
        s.seen.push((method.to_owned(), path.clone()));
        let hit = |r: &Rule| r.method == method && path.starts_with(&r.prefix);
        if let Some(i) = s.rules.iter().rposition(|r| r.once && hit(r)) {
            return s.rules.remove(i).reply;
        }
        if let Some(r) = s.rules.iter().rev().find(|r| !r.once && hit(r)) {
            return r.reply.clone();
        }
        match (method, path.as_str()) {
            ("POST", "/session") => Reply::Json(200, json!({"sessionId": "S", "value": {"sessionId": "S"}})),
            _ => Reply::Json(404, json!({"value": {"error": "unknown command", "message": path}})),
        }
    }
}

fn strip_session(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("/session/") {
        return match rest.find('/') {
            Some(i) => rest[i..].to_owned(),
            None => "/session".to_owned(),
        };
    }
    path.to_owned()
}

impl Transport for ScriptedWda {
    async fn screenshot_jpeg(&self) -> std::io::Result<Option<Vec<u8>>> {
        let mut s = self.0.lock().unwrap();
        s.jpeg_reads += 1;
        match s.jpeg.clone() {
            None => Ok(None),
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(kind)) => Err(std::io::Error::new(kind, "fake MJPEG failure")),
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        _body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> std::io::Result<(u16, Vec<u8>)> {
        match self.reply(method, path) {
            Reply::Json(status, v) => Ok((status, v.to_string().into_bytes())),
            Reply::Fail(kind) => Err(std::io::Error::new(kind, format!("fake {kind:?}: {method} {path}"))),
            Reply::Hang => {
                tokio::time::sleep(timeout).await;
                Err(std::io::Error::new(ErrorKind::TimedOut, format!("fake hang: {method} {path}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WdaClient, WdaError};

    #[tokio::test]
    async fn scripted_replies_are_matched_by_route_and_once_rules_win() {
        let w = ScriptedWda::new();
        w.on("GET", "/wda/activeAppInfo", Reply::value(json!({"bundleId": "a"})));
        w.once("GET", "/wda/activeAppInfo", Reply::unavailable());
        let c = WdaClient::new(w.clone());
        assert!(matches!(c.active_app().await, Err(WdaError::Unavailable(_))));
        assert_eq!(c.active_app().await.unwrap(), "a");
        assert_eq!(w.count("GET", "/wda/activeAppInfo"), 2);
        assert_eq!(strip_session("/session/S/actions"), "/actions");
        assert_eq!(strip_session("/session/abc"), "/session");
    }
}
