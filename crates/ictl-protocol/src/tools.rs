//! 工具调用（工具名 + JSON 参数）→ 守护进程 [`Request`]。MCP 和网关共用这一份参数检查。
//!
//! 两种口径（[`ArgPolicy`]）只在「怎么读参数」和「允许打开哪些网址」上不同，取值范围一律按严格的来：
//! - [`ArgPolicy::Compatible`]：本机 MCP。与 toolkit 一致，写成字符串的数字、`"true"`/`"false"` 照收；
//!   参数里还有 `then` 等 MCP 自己的键，多出来的键不管；任何 URL Scheme 都能打开（打开别的 App 要用）
//! - [`ArgPolicy::Strict`]：网关（参数来自远端模型）。只收 JSON 原生类型，有不认识的键就拒绝；
//!   网址只许 http/https/tel/sms/mailto
//!
//! 出错时给 [`ToolArgError`]：[`std::fmt::Display`] 是给 agent 看的本地化文字，[`ToolArgError::code_message`]
//! 是固定的英文（网关按它的开头认错误种类）。

use serde_json::{Map, Value};

use crate::Request;

/// 坐标、区域的上限（点）。真机屏幕最大一千多点，留足余量；挡住 1e300 之类的值
pub const MAX_COORDINATE: f64 = 100_000.0;
/// 长按、滑动的时长上限（秒），同守护进程的检查
pub const MAX_GESTURE_SECONDS: f64 = 10.0;
/// 列元素最多列多少个
pub const MAX_LIMIT: u64 = 1000;
pub const MAX_INDEX: u64 = 10_000;
/// 文字参数（输入的文字、filter、按名字点的文字）的字符数上限
pub const MAX_TEXT: usize = 10_000;
pub const MAX_APP: usize = 512;
pub const MAX_URL: usize = 4096;
/// `iphone_wait_for` 的 timeout 上限（秒）
pub const MAX_WAIT_SECONDS: f64 = 30.0;
/// `iphone_press_button` 能按的键。`back` 只有安卓有，网关自己转过去；守护进程收到会报错
pub const BUTTONS: [&str; 5] = ["home", "back", "volume_up", "volume_down", "lock"];
/// [`ArgPolicy::Strict`] 下能打开的网址
pub const REMOTE_URL_SCHEMES: [&str; 5] = ["http", "https", "tel", "sms", "mailto"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgPolicy {
    /// 本机 MCP（见模块说明）
    Compatible,
    /// 网关（见模块说明）
    Strict,
}

/// 参数要的类型（出错说明用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    Number,
    Integer,
    Text,
    Bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolArgError {
    UnknownTool(String),
    /// [`ArgPolicy::Strict`] 下不认识的参数
    UnknownArgument(String),
    Missing {
        key: &'static str,
        kind: ArgKind,
    },
    WrongType {
        key: &'static str,
        kind: ArgKind,
    },
    OutOfRange {
        key: &'static str,
        min: f64,
        max: f64,
    },
    TooLong {
        key: &'static str,
        max: usize,
    },
    Empty(&'static str),
    /// 区域放大的 x、y、width、height 没给全
    RegionIncomplete,
    /// 区域宽或高是 0
    RegionSize,
    InvalidButton(String),
    InvalidUrl,
    UnsupportedUrlScheme(String),
}

impl ToolArgError {
    /// 固定的英文说明（网关回给远端模型；它按 `Unknown or unsupported tool` 的开头认出是工具名不对）
    pub fn code_message(&self) -> String {
        match self {
            Self::UnknownTool(t) => format!("Unknown or unsupported tool {t}"),
            Self::UnknownArgument(_) => "Unknown command argument".into(),
            Self::Missing { key, .. }
            | Self::WrongType { key, .. }
            | Self::OutOfRange { key, .. }
            | Self::TooLong { key, .. }
            | Self::Empty(key) => format!("Invalid {key}"),
            Self::RegionIncomplete | Self::RegionSize => "Invalid screenshot dimensions".into(),
            Self::InvalidButton(_) => "Invalid button".into(),
            Self::InvalidUrl => "Invalid URL".into(),
            Self::UnsupportedUrlScheme(_) => "Unsupported URL scheme".into(),
        }
    }
}

impl std::fmt::Display for ToolArgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use ictl_l10n::{tr, trf};
        let text = match self {
            Self::UnknownTool(t) => trf!("未知工具 {t}", "unknown tool {t}"),
            Self::UnknownArgument(k) => trf!("不认识的参数 {k}", "unknown parameter {k}"),
            Self::Missing { key, kind: ArgKind::Number | ArgKind::Integer } => {
                trf!("缺少数字参数 {key}", "missing number parameter {key}")
            }
            Self::Missing { key, kind: ArgKind::Text } => trf!("缺少文字参数 {key}", "missing text parameter {key}"),
            Self::Missing { key, kind: ArgKind::Bool } => {
                trf!("缺少参数 {key}(true 或 false)", "missing parameter {key} (true or false)")
            }
            Self::WrongType { key, kind: ArgKind::Number } => {
                trf!("参数 {key} 要是数字", "parameter {key} must be a number")
            }
            Self::WrongType { key, kind: ArgKind::Integer } => {
                trf!("参数 {key} 要是非负整数", "parameter {key} must be a non-negative integer")
            }
            Self::WrongType { key, kind: ArgKind::Text } => trf!("参数 {key} 要是文字", "parameter {key} must be text"),
            Self::WrongType { key, kind: ArgKind::Bool } => {
                trf!("参数 {key} 要是 true 或 false", "parameter {key} must be true or false")
            }
            Self::OutOfRange { key, min, max } => {
                trf!("参数 {key} 要在 {min} 到 {max} 之间", "parameter {key} must be between {min} and {max}")
            }
            Self::TooLong { key, max } => {
                trf!("参数 {key} 太长(最多 {max} 个字符)", "parameter {key} is too long (at most {max} characters)")
            }
            Self::Empty(key) => trf!("参数 {key} 不能为空", "parameter {key} can't be empty"),
            Self::RegionIncomplete => {
                tr!("放大区域要同时给出 x、y、width、height", "a zoom region needs all of x, y, width and height")
                    .into()
            }
            Self::RegionSize => {
                tr!("放大区域的宽、高要大于 0", "a zoom region needs a width and height above 0").into()
            }
            Self::InvalidButton(_) => tr!(
                "button 只能是 home / volume_up / volume_down / lock",
                "button must be home / volume_up / volume_down / lock"
            )
            .into(),
            Self::InvalidUrl => {
                tr!("url 不是有效的网址或 URL Scheme", "url is not a valid address or URL scheme").into()
            }
            Self::UnsupportedUrlScheme(s) => trf!(
                "不能打开 {s}: 网址只能是 {}",
                "can't open {s}: only {} addresses are allowed",
                REMOTE_URL_SCHEMES.join(" / ")
            ),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for ToolArgError {}

struct Args<'a> {
    values: &'a Map<String, Value>,
    policy: ArgPolicy,
    used: Vec<&'static str>,
}

impl Args<'_> {
    /// 没给或给了 null 都算没给
    fn get(&mut self, key: &'static str) -> Option<&Value> {
        self.used.push(key);
        self.values.get(key).filter(|v| !v.is_null())
    }

    fn strict(&self) -> bool {
        self.policy == ArgPolicy::Strict
    }

    fn opt_number(&mut self, key: &'static str, kind: ArgKind) -> Result<Option<f64>, ToolArgError> {
        let strict = self.strict();
        let Some(v) = self.get(key) else { return Ok(None) };
        let n = match v {
            Value::Number(n) => n.as_f64(),
            // toolkit 的 pydantic 也收 "3"
            Value::String(s) if !strict => s.trim().parse().ok(),
            _ => None,
        };
        n.filter(|x: &f64| x.is_finite()).map(Some).ok_or(ToolArgError::WrongType { key, kind })
    }

    fn number_in(&mut self, key: &'static str, default: Option<f64>, max: f64) -> Result<f64, ToolArgError> {
        let n = self
            .opt_number(key, ArgKind::Number)?
            .or(default)
            .ok_or(ToolArgError::Missing { key, kind: ArgKind::Number })?;
        if !(0.0..=max).contains(&n) {
            return Err(ToolArgError::OutOfRange { key, min: 0.0, max });
        }
        Ok(n)
    }

    fn coordinate(&mut self, key: &'static str) -> Result<f64, ToolArgError> {
        self.number_in(key, None, MAX_COORDINATE)
    }

    fn integer(&mut self, key: &'static str, default: u64, max: u64) -> Result<u64, ToolArgError> {
        let Some(n) = self.opt_number(key, ArgKind::Integer)? else { return Ok(default) };
        if n < 0.0 || n.fract() != 0.0 {
            return Err(ToolArgError::WrongType { key, kind: ArgKind::Integer });
        }
        if n > max as f64 {
            return Err(ToolArgError::OutOfRange { key, min: 0.0, max: max as f64 });
        }
        Ok(n as u64)
    }

    fn text(&mut self, key: &'static str, default: Option<&str>, max: usize) -> Result<String, ToolArgError> {
        let s = match self.get(key) {
            None => default.ok_or(ToolArgError::Missing { key, kind: ArgKind::Text })?.to_owned(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(ToolArgError::WrongType { key, kind: ArgKind::Text }),
        };
        if s.chars().count() > max {
            return Err(ToolArgError::TooLong { key, max });
        }
        if s.contains('\0') {
            return Err(ToolArgError::WrongType { key, kind: ArgKind::Text });
        }
        Ok(s)
    }

    fn boolean(&mut self, key: &'static str) -> Result<bool, ToolArgError> {
        let strict = self.strict();
        match self.get(key) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if !strict && s.eq_ignore_ascii_case("true") => Ok(true),
            Some(Value::String(s)) if !strict && s.eq_ignore_ascii_case("false") => Ok(false),
            Some(_) => Err(ToolArgError::WrongType { key, kind: ArgKind::Bool }),
        }
    }
}

/// 网址或 URL Scheme 的 scheme（小写）。没有 scheme、含控制字符、http(s) 没有主机名的返回 None
fn url_scheme(url: &str) -> Option<String> {
    if url.chars().any(char::is_control) {
        return None;
    }
    let (scheme, rest) = url.split_once(':')?;
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() || !chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    if matches!(scheme.as_str(), "http" | "https") {
        let host = rest.strip_prefix("//")?;
        if host.is_empty() || host.starts_with(['/', '?', '#']) {
            return None;
        }
    }
    Some(scheme)
}

/// 工具调用 → 守护进程请求。工具名是 MCP 的（`iphone_tap` 等）；批量、`then` 这些 MCP 自己处理的不在这里
pub fn tool_to_request(tool: &str, args: &Map<String, Value>, policy: ArgPolicy) -> Result<Request, ToolArgError> {
    let mut a = Args { values: args, policy, used: Vec::new() };
    let req = match tool {
        "iphone_status" => Request::Status,
        "iphone_screenshot" => {
            let keys = ["x", "y", "width", "height"];
            let given: Vec<Option<f64>> =
                keys.iter().map(|k| a.opt_number(k, ArgKind::Number)).collect::<Result<_, _>>()?;
            let region = match given[..] {
                [None, None, None, None] => None,
                [Some(_), Some(_), Some(_), Some(_)] => {
                    let r: Vec<f64> = keys.iter().map(|k| a.coordinate(k)).collect::<Result<_, _>>()?;
                    if r[2] <= 0.0 || r[3] <= 0.0 {
                        return Err(ToolArgError::RegionSize);
                    }
                    Some((r[0], r[1], r[2], r[3]))
                }
                _ => return Err(ToolArgError::RegionIncomplete),
            };
            Request::Screenshot { region }
        }
        "iphone_list_elements" => Request::ListElements {
            filter: a.text("filter", Some(""), MAX_TEXT)?,
            limit: a.integer("limit", 150, MAX_LIMIT)? as usize,
            max_age_ms: 0,
            snapshot_timeout_ms: 0,
        },
        "iphone_tap" => Request::Tap { x: a.coordinate("x")?, y: a.coordinate("y")? },
        "iphone_tap_element" => Request::TapElement {
            text: a.text("text", None, MAX_TEXT)?,
            index: a.integer("index", 0, MAX_INDEX)? as usize,
            max_age_ms: 0,
        },
        "iphone_long_press" => Request::LongPress {
            x: a.coordinate("x")?,
            y: a.coordinate("y")?,
            seconds: a.number_in("seconds", Some(1.0), MAX_GESTURE_SECONDS)?,
        },
        "iphone_swipe" => Request::Swipe {
            from: (a.coordinate("from_x")?, a.coordinate("from_y")?),
            to: (a.coordinate("to_x")?, a.coordinate("to_y")?),
            seconds: a.number_in("seconds", Some(0.3), MAX_GESTURE_SECONDS)?,
        },
        "iphone_type_text" => Request::TypeText { text: a.text("text", None, MAX_TEXT)?, submit: a.boolean("submit")? },
        "iphone_press_button" => {
            let button = a.text("button", None, MAX_TEXT)?;
            if !BUTTONS.contains(&button.as_str()) {
                return Err(ToolArgError::InvalidButton(button));
            }
            Request::PressButton { button }
        }
        "iphone_list_apps" => Request::ListApps { include_system: a.boolean("include_system")? },
        "iphone_launch_app" | "iphone_terminate_app" => {
            let app = a.text("app", None, MAX_APP)?;
            if app.trim().is_empty() {
                return Err(ToolArgError::Empty("app"));
            }
            if tool == "iphone_launch_app" { Request::LaunchApp { app } } else { Request::TerminateApp { app } }
        }
        "iphone_open_url" => {
            let url = a.text("url", None, MAX_URL)?;
            let scheme = url_scheme(&url).ok_or(ToolArgError::InvalidUrl)?;
            if policy == ArgPolicy::Strict && !REMOTE_URL_SCHEMES.contains(&scheme.as_str()) {
                return Err(ToolArgError::UnsupportedUrlScheme(scheme));
            }
            Request::OpenUrl { url }
        }
        "iphone_wait_for" => {
            let secs = a.number_in("timeout", Some(5.0), MAX_WAIT_SECONDS)?;
            let text = a.text("text", None, MAX_TEXT)?;
            if text.is_empty() {
                return Err(ToolArgError::Empty("text"));
            }
            Request::WaitFor { text, timeout_ms: (secs * 1000.0).floor() as u64 }
        }
        "iphone_list_devices" => Request::ListDevices,
        "iphone_select_device" => Request::SelectDevice { device: a.text("device", None, MAX_APP)? },
        other => return Err(ToolArgError::UnknownTool(other.to_owned())),
    };
    if policy == ArgPolicy::Strict
        && let Some(k) = args.keys().find(|k| !a.used.contains(&k.as_str()))
    {
        return Err(ToolArgError::UnknownArgument(k.clone()));
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(tool: &str, v: Value, policy: ArgPolicy) -> Result<Request, ToolArgError> {
        tool_to_request(tool, v.as_object().unwrap(), policy)
    }

    #[test]
    fn values_are_bounded_under_both_policies() {
        for policy in [ArgPolicy::Compatible, ArgPolicy::Strict] {
            assert_eq!(call("iphone_tap", json!({"x": 2, "y": 3}), policy), Ok(Request::Tap { x: 2.0, y: 3.0 }));
            for bad in [json!({"x": -1, "y": 3}), json!({"x": 1e300, "y": 3}), json!({"x": 1})] {
                assert!(call("iphone_tap", bad, policy).is_err());
            }
            assert!(call("iphone_list_elements", json!({"limit": 1001}), policy).is_err());
            assert!(call("iphone_list_elements", json!({"limit": 1.5}), policy).is_err());
            assert!(call("iphone_tap_element", json!({"text": "x", "index": -1}), policy).is_err());
            assert!(call("iphone_long_press", json!({"x": 1, "y": 1, "seconds": 11}), policy).is_err());
            assert!(call("iphone_wait_for", json!({"text": "x", "timeout": 31}), policy).is_err());
            assert!(call("iphone_wait_for", json!({"text": ""}), policy).is_err());
            assert!(call("iphone_launch_app", json!({"app": "  "}), policy).is_err());
            assert!(call("iphone_press_button", json!({"button": "power"}), policy).is_err());
            assert_eq!(call("iphone_screenshot", json!({"x": 1}), policy), Err(ToolArgError::RegionIncomplete));
            assert_eq!(
                call("iphone_screenshot", json!({"x": 1, "y": 1, "width": 0, "height": 5}), policy),
                Err(ToolArgError::RegionSize)
            );
            assert!(call("iphone_open_url", json!({"url": "not a url"}), policy).is_err());
            assert!(call("iphone_open_url", json!({"url": "https://"}), policy).is_err());
            assert!(call("iphone_nope", json!({}), policy).is_err());
        }
    }

    #[test]
    fn strict_policy_takes_native_types_known_keys_and_web_urls_only() {
        let s = ArgPolicy::Strict;
        assert!(call("iphone_tap", json!({"x": "2", "y": 3}), s).is_err());
        assert!(call("iphone_tap", json!({"x": 2, "y": 3, "extra": true}), s).is_err());
        assert!(call("iphone_type_text", json!({"text": "a", "submit": "true"}), s).is_err());
        assert!(call("iphone_open_url", json!({"url": "file:///etc/passwd"}), s).is_err());
        assert!(call("iphone_open_url", json!({"url": "weixin://"}), s).is_err());
        assert!(call("iphone_open_url", json!({"url": "HTTPS://example.com"}), s).is_ok());
        assert_eq!(
            call("iphone_tap", json!({"x": 2, "y": 3, "extra": true}), s).unwrap_err().code_message(),
            "Unknown command argument"
        );
        assert!(call("iphone_x", json!({}), s).unwrap_err().code_message().starts_with("Unknown or unsupported tool"));
        // null 算没给
        assert!(call("iphone_list_elements", json!({"limit": null}), s).is_ok());
    }

    #[test]
    fn compatible_policy_keeps_toolkit_coercions_and_app_url_schemes() {
        let c = ArgPolicy::Compatible;
        assert_eq!(
            call("iphone_tap_element", json!({"text": "设置", "index": "2", "then": "elements"}), c),
            Ok(Request::TapElement { text: "设置".into(), index: 2, max_age_ms: 0 })
        );
        assert_eq!(
            call("iphone_type_text", json!({"text": "a", "submit": "TRUE"}), c),
            Ok(Request::TypeText { text: "a".into(), submit: true })
        );
        for url in ["weixin://", "prefs:root=General", "https://example.com/a?b#c"] {
            assert!(call("iphone_open_url", json!({ "url": url }), c).is_ok(), "{url}");
        }
    }

    #[test]
    fn errors_have_stable_codes_and_localized_text() {
        let e = ToolArgError::Missing { key: "x", kind: ArgKind::Number };
        assert_eq!(e.code_message(), "Invalid x");
        assert_eq!(ictl_l10n::with_lang(ictl_l10n::Lang::Zh, || e.to_string()), "缺少数字参数 x");
        assert_eq!(ictl_l10n::with_lang(ictl_l10n::Lang::En, || e.to_string()), "missing number parameter x");
    }
}
