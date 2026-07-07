//! 使用者接管活動偵測——純邏輯（不含任何 NSEvent / unsafe / FFI）。
//!
//! Edit Guard（`docs/design/edit-guard.md`）v1 的判定：錄音期間，凡是 raflow **自己
//! 從不注入**的事件即視為「使用者接管」——
//! - 滑鼠按下（由 `nsevent_monitor` 以事件遮罩處理，不經本模組）；
//! - 導覽鍵：方向鍵、Home/End、PageUp/PageDown。
//!
//! v2（打字接管）：透過 `kCGEventSourceUserData` 自我濾除——raflow 注入的事件都帶
//! [`RAFLOW_INJECT_MARKER`] 標記，故可安全地把**任何真正的使用者按鍵**（打字/標點/Enter/
//! 導覽…）當接管信號。分類邏輯 `key_is_user_takeover` 為純函式，unit test 純 Rust 跑通
//! （ADR-0004 要求純邏輯與 unsafe 分離；欄位讀取的 FFI 在 `nsevent_monitor`）。

use raflow_core::RAFLOW_INJECT_MARKER;

/// macOS Carbon `kVK_*` virtual key codes（HIToolbox `Events.h`）——導覽鍵集合。
/// 這些是實體按鍵位置碼，與鍵盤佈局／輸入法無關，可安全硬編。
const KVK_LEFT_ARROW: u16 = 0x7B;
const KVK_RIGHT_ARROW: u16 = 0x7C;
const KVK_DOWN_ARROW: u16 = 0x7D;
const KVK_UP_ARROW: u16 = 0x7E;
const KVK_HOME: u16 = 0x73;
const KVK_END: u16 = 0x77;
const KVK_PAGE_UP: u16 = 0x74;
const KVK_PAGE_DOWN: u16 = 0x79;

/// 此 key code 是否為「游標導覽鍵」（方向鍵 / Home / End / PageUp / PageDown）。
///
/// 用於 `key_is_user_takeover` 的 fallback 分支（讀不到 `userData` 時的安全子集）：這些鍵
/// raflow **從不注入**，故即使無法自我濾除也零誤判。
pub fn is_navigation_key(key_code: u16) -> bool {
    matches!(
        key_code,
        KVK_LEFT_ARROW
            | KVK_RIGHT_ARROW
            | KVK_DOWN_ARROW
            | KVK_UP_ARROW
            | KVK_HOME
            | KVK_END
            | KVK_PAGE_UP
            | KVK_PAGE_DOWN
    )
}

/// 給定一個 `KeyDown` 事件的 `kCGEventSourceUserData`（讀不到為 `None`）與 keyCode，
/// 判定是否為「使用者接管」——即真正的使用者輸入，而非 raflow 自己 enigo 注入的按鍵。
///
/// 自我濾除（設計 §3 v2）：raflow 注入的所有事件都帶 [`RAFLOW_INJECT_MARKER`] 標記，
/// 硬體（真人）事件的 `userData` 為 `0`。故：
/// - `Some(RAFLOW_INJECT_MARKER)` → raflow 自身注入 → **不算接管**（`false`）。
/// - `Some(_其他)` → 真正的使用者按鍵（打字/標點/Enter/導覽…任何鍵）→ **接管**（`true`）。
/// - `None`（無法取得 CGEvent/欄位）→ 退回安全子集：只認導覽鍵（raflow 不注入 → 零誤判）。
pub fn key_is_user_takeover(user_data: Option<i64>, key_code: u16) -> bool {
    match user_data {
        Some(d) if d == RAFLOW_INJECT_MARKER => false,
        Some(_) => true,
        None => is_navigation_key(key_code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 決策表：導覽鍵 → true；一般可列印字元 / Backspace / 修飾鍵 → false。
    #[test]
    fn navigation_key_decision_table() {
        let cases: &[(&str, u16, bool)] = &[
            // 導覽鍵（接管信號）
            ("left arrow", 0x7B, true),
            ("right arrow", 0x7C, true),
            ("down arrow", 0x7D, true),
            ("up arrow", 0x7E, true),
            ("home", 0x73, true),
            ("end", 0x77, true),
            ("page up", 0x74, true),
            ("page down", 0x79, true),
            // 非導覽鍵（raflow 可能注入或無關 → 不接管）
            ("'a' key", 0x00, false),
            ("'s' key", 0x01, false),
            ("space", 0x31, false),
            ("delete/backspace", 0x33, false),
            ("return", 0x24, false),
            ("escape", 0x35, false),
        ];
        for (label, code, expected) in cases {
            assert_eq!(
                is_navigation_key(*code),
                *expected,
                "{label}: key_code={code:#x}"
            );
        }
    }

    /// 決策表：自我濾除 + 「任何使用者按鍵皆接管」。
    #[test]
    fn key_is_user_takeover_decision_table() {
        // (label, user_data, key_code, expect_takeover)
        let cases: &[(&str, Option<i64>, u16, bool)] = &[
            // raflow 自身注入（帶標記）→ 不接管，無論哪個鍵
            ("raflow-injected 'a'", Some(RAFLOW_INJECT_MARKER), 0x00, false),
            ("raflow-injected newline", Some(RAFLOW_INJECT_MARKER), 0x24, false),
            ("raflow-injected backspace", Some(RAFLOW_INJECT_MARKER), 0x33, false),
            // 真正使用者輸入（硬體 userData=0）→ 任何鍵都接管
            ("user types 'a'", Some(0), 0x00, true),
            ("user types comma", Some(0), 0x2B, true),
            ("user Shift+Enter (Return)", Some(0), 0x24, true),
            ("user backspace", Some(0), 0x33, true),
            ("user left arrow", Some(0), 0x7B, true),
            // 其他來源（非 raflow 標記、非 0）→ 仍視為使用者/外部 → 接管
            ("other injector", Some(100), 0x00, true),
            // 讀不到 userData → 退回導覽鍵安全子集
            ("no userdata, nav key", None, 0x7C, true),
            ("no userdata, printable", None, 0x00, false),
            ("no userdata, return", None, 0x24, false),
        ];
        for (label, ud, code, expected) in cases {
            assert_eq!(
                key_is_user_takeover(*ud, *code),
                *expected,
                "{label}: user_data={ud:?} key_code={code:#x}"
            );
        }
    }
}
