//! raflow-hotkey：透過 NSEvent global monitor 偵測「雙擊 Cmd」作為錄音 toggle。
//!
//! - 純邏輯狀態機放在 `double_tap` 模組（無 unsafe）
//! - macOS NSEvent FFI 集中於 `nsevent_monitor` 模組（ADR-0004）
//!
//! 對外 API：
//! - `register(tx) -> Result<HotkeyHandle, RaflowError>`：必須於主執行緒呼叫
//! - `HotkeyHandle` drop 時自動 removeMonitor

mod double_tap;

#[cfg(target_os = "macos")]
mod nsevent_monitor;

#[cfg(target_os = "macos")]
pub use nsevent_monitor::{HotkeyHandle, register};
