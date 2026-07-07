pub mod audio;
pub mod error;
pub mod hotkey;
pub mod transcript;

pub use audio::AudioFrame;
pub use error::RaflowError;
pub use hotkey::HotkeyEvent;
pub use transcript::TranscriptUpdate;

/// raflow 透過 enigo 注入的所有事件，其 macOS `kCGEventSourceUserData`（CGEventField 42）
/// 皆標記為此值（見 `EnigoBackend::new` 設定 `Settings.event_source_user_data`）。
///
/// Edit Guard 的使用者活動監看（`raflow-hotkey`）據此**自我濾除**：帶此標記的按鍵是 raflow
/// 自己注入的，不算接管；其餘（`userData==0` 的硬體事件）才是**真正的使用者輸入**。這讓
/// 「打字/標點/Enter…也算接管」得以零誤判（否則 raflow 每注入一個字都會被誤判成使用者接管）。
///
/// 值需與 `0`（一般硬體事件預設）及 enigo 內建預設 `100` 區隔。此處取 ASCII "raflow" 位元組。
pub const RAFLOW_INJECT_MARKER: i64 = 0x7261_666C_6F77; // b"raflow" big-endian
