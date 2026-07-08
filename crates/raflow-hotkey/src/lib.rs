//! raflow-hotkey：透過 NSEvent global monitor 偵測「雙擊 Cmd」作為錄音 toggle。
//!
//! - 純邏輯狀態機放在 `double_tap` 模組（無 unsafe）
//! - macOS NSEvent FFI 集中於 `nsevent_monitor` 模組（ADR-0004）
//!
//! 對外 API：
//! - `register(tx, on_toggle) -> Result<HotkeyHandle, RaflowError>`：必須於主執行緒呼叫；
//!   `on_toggle` 於每次雙擊命中時在**主執行緒**被呼叫（供主執行緒取樣，如 TIS 讀輸入法，ADR-0007）
//! - `HotkeyHandle` drop 時自動 removeMonitor
//! - `register_activity_monitor(tx) -> Result<ActivityMonitorHandle, RaflowError>`：
//!   Edit Guard v1——錄音期間監看「使用者接管」信號（滑鼠按下 / 導覽鍵），
//!   drop 時自動 removeMonitor。純判定邏輯放在 `activity` 模組（無 unsafe）。

mod activity;
mod double_tap;

#[cfg(target_os = "macos")]
mod nsevent_monitor;

#[cfg(target_os = "macos")]
pub use nsevent_monitor::{ActivityMonitorHandle, HotkeyHandle, register, register_activity_monitor};
