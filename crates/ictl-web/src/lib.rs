#[cfg(target_arch = "wasm32")]
mod actions;
#[cfg(target_arch = "wasm32")]
pub mod android;
pub mod android_text;
#[cfg(target_arch = "wasm32")]
mod client;
pub mod identity;
pub mod interaction;
#[cfg(target_arch = "wasm32")]
mod js;
pub mod packets;
#[cfg(target_arch = "wasm32")]
mod polling;
#[cfg(target_arch = "wasm32")]
pub use interaction::{pointer_command_js, to_phone_point_js};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = canonicalUID)]
pub fn canonical_uid_js(value: JsValue) -> Result<String, JsValue> {
    identity::canonical_uid(&value.as_string().ok_or_else(|| js::error("Invalid device UID"))?).map_err(js::error)
}
#[cfg(target_arch = "wasm32")]
pub use android::*;
#[cfg(target_arch = "wasm32")]
pub use client::BrowserClient;

#[cfg(target_arch = "wasm32")]
mod ui;
#[cfg(target_arch = "wasm32")]
pub use ui::mount;
