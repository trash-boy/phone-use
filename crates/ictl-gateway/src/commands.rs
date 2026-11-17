use crate::error::{ApiError, Result};
use ictl_protocol::{ArgPolicy, tool_to_request};
use serde_json::Value;

/// 网关不提供的工具：切换设备只能在本机做
const UNSUPPORTED: [&str; 1] = ["iphone_select_device"];

/// 远端模型的工具调用 → 守护进程请求（JSON）。参数检查和 MCP 共用 `ictl_protocol::tool_to_request`，
/// 按严格口径：只收 JSON 原生类型、不认识的参数报错、网址只许 http/https/tel/sms/mailto
pub fn command_request(tool: &str, args: &Value) -> Result<Value> {
    let values = args.as_object().ok_or_else(|| ApiError::new(400, "Invalid command args"))?;
    if UNSUPPORTED.contains(&tool) {
        return Err(ApiError::new(400, format!("Unknown or unsupported tool {tool}")));
    }
    let request =
        tool_to_request(tool, values, ArgPolicy::Strict).map_err(|error| ApiError::new(400, error.code_message()))?;
    serde_json::to_value(request).map_err(|_| ApiError::new(400, "Invalid command args"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_protocol_and_limits() {
        assert_eq!(command_request("iphone_tap", &json!({"x":2,"y":3})).unwrap()["op"], "tap");
        for v in [json!({"x":-1,"y":3}), json!({"x":2,"y":3,"extra":true}), json!({"x":"2","y":3})] {
            assert!(command_request("iphone_tap", &v).is_err());
        }
        assert!(command_request("iphone_screenshot", &json!({"x":1})).is_err());
        assert!(command_request("iphone_open_url", &json!({"url":"file:///etc/passwd"})).is_err());
        assert!(command_request("iphone_select_device", &json!({"device":"x"})).is_err());
        assert_eq!(command_request("iphone_press_button", &json!({"button":"back"})).unwrap()["button"], "back");
        let unknown = command_request("iphone_unknown", &json!({})).unwrap_err();
        assert!(unknown.message.starts_with("Unknown or unsupported tool"), "{}", unknown.message);
        assert_eq!(command_request("iphone_tap", &json!({"y":3})).unwrap_err().message, "Invalid x");
    }
}
