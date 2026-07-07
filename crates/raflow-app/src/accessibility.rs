//! Phase 6b-fix：用 macOS Accessibility API 偵測「目前 focus 是不是文字輸入元件」。
//!
//! 用途：raflow 收到雙擊 Cmd 開始錄音時，main thread 立刻 query 一次 focus，存成
//! `bool` 給整個 session 用。printer 收到 partial / final 時：
//! - **focus 在輸入框** → enigo 直接 inject + menu bar 顯示截斷版（夠用）；不彈 floating panel
//! - **focus 不在輸入框** → enigo 仍會 inject 但會跑去敲別處（已知 spec/input.md §1
//!   limitation，無法靠 macOS public API 阻止），但 floating panel 顯示完整文字作為
//!   視覺安全網讓使用者看到自己說了什麼
//!
//! 另提供 [`frontmost_app_pid`]：app 級 PID 查詢，供注入焦點守衛（spec/input.md §7d）
//! 在「錄音中切 app」時停止注入——與上述元件級偵測互補，不判斷元件是否可輸入。
//!
//! ## 偵測邏輯（三狀態）
//!
//! 1. **`Untrusted`**：raflow 沒拿到 macOS Accessibility 權限——AX API 全部 disabled。
//!    enigo 走 CGEventPost（只需 Input Monitoring），所以 inject 仍能用，但 AX query
//!    一律 fail。**此情境視為 editable**（panel 不彈），避免在 inject 已成功的場景多
//!    疊一個面板擾人；同時 stderr 提示去開權限。
//! 2. **`Unknown`**：有權限但 query 不到 focused element（極少見：focus 在 menu /
//!    系統元件 / race condition）。**視為非 editable**，彈 panel 作安全網。
//! 3. **`Detected`**：query 成功，依雙重訊號判斷：
//!    - **AXRole** 命中 [`EDITABLE_ROLES`] → editable（cover 原生 AppKit / Safari / 多數 web）
//!    - **AXSelectedTextRange** 屬性 settable → editable（fallback；該屬性只存在於可
//!      編輯文字元件，cover Chromium lazy AX tree 把 web input 回成 AXGroup 等情境）
//!
//! 對外公開 API 完全 safe；unsafe FFI 集中在私有 helper。詳見 ADR-0005（範圍已擴及 accessibility.rs）。

use objc2_application_services::{
    AXError, AXIsProcessTrusted, AXIsProcessTrustedWithOptions, AXUIElement,
    kAXTrustedCheckOptionPrompt,
};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFRetained, CFString, CFType, kCFBooleanTrue,
};
use std::ptr::NonNull;

/// 視為「文字輸入框」的 AX role（Apple `AXRoleConstants.h` ABI 字串）。
const EDITABLE_ROLES: &[&str] = &[
    "AXTextField",
    "AXTextArea",
    "AXSearchField",
    "AXSecureTextField",
    "AXComboBox",
];

/// AX 偵測成功時的單一元件資訊。`role` 對診斷有用。
#[derive(Debug, Clone)]
pub struct FocusInfo {
    /// 焦點元件的 `AXRole` 字串；查不到時填 `"<unknown>"`。
    pub role: String,
    /// 是否視為文字輸入元件（決定 floating panel 是否抑制）。
    pub editable: bool,
}

/// 系統當下 focused element 的偵測結果。三狀態請見 module 文件。
#[derive(Debug, Clone)]
pub enum FocusDetection {
    /// raflow 未取得 Accessibility 權限——AX API disabled。
    Untrusted,
    /// 有權限但拿不到 focused element（極少見）。
    Unknown,
    /// 拿到 focused element 並完成判定。
    Detected(FocusInfo),
}

impl FocusDetection {
    /// 是否抑制 floating panel。
    /// - `Untrusted` → true（沒 Accessibility 權限；inject 仍能用，panel 多餘）
    /// - `Unknown` → true（Electron / 隱藏 AX tree 的 app；實測這類場景 inject 多半工作，
    ///   panel 跑出來反而干擾）
    /// - `Detected` → 依 `editable`（明確判斷後才決定）
    ///
    /// 設計取捨：原 spec 主張 fallback 時「寧可多顯示」當安全網，但實機 Electron 系 app
    /// （ChatGPT desktop / Slack / Discord 等）的 AX query 一律回 nil → 永遠落到 Unknown
    /// → panel 永遠彈，跟 inject 雙重顯示干擾。改採「只在明確判斷為非輸入框時才彈」。
    /// 真正在桌面 / Finder 講話的場景仍有 clipboard fallback（Cmd+V）保底。
    pub fn suppresses_panel(&self) -> bool {
        match self {
            Self::Untrusted | Self::Unknown => true,
            Self::Detected(info) => info.editable,
        }
    }
}

/// raflow 是否拿到 Accessibility 權限。`AXIsProcessTrusted()` 不會跳系統 prompt，
/// 純查詢 TCC database。詳見 module 文件。
pub fn is_trusted() -> bool {
    // SAFETY: AXIsProcessTrusted 為 Apple public ABI，無前置條件，任意執行緒可呼叫，
    // 不接任何指標引數，回 bool。
    unsafe { AXIsProcessTrusted() }
}

/// 啟動時呼叫一次：若沒拿到 Accessibility，主動觸發系統的「raflow 想要控制這台電腦…」
/// dialog 並引導使用者去「系統設定 → 隱私權與安全性 → 輔助使用」打勾。回傳值同
/// `is_trusted()`（呼叫當下是否已 trusted）。
///
/// **設計原因**：enigo 的 `CGEventPost` 在沒 Accessibility 時**靜默失敗不報錯**，
/// 使用者會看到「menu bar 變紅但輸入框沒文字」這種完全沒線索的故障。Apple 提供的
/// `AXIsProcessTrustedWithOptions(prompt: true)` 是業界標準解法：未授權時自動跳
/// 系統 dialog，比我們自己 eprintln 引導文字有效得多。
///
/// **macOS 行為注意**：dialog 一個 process 生命週期只跳一次，因此本函式設計成「啟動時
/// 呼叫一次」。後續執行期的查詢請用便宜的 `is_trusted()`（不再 prompt）。
pub fn ensure_trusted_with_prompt() -> bool {
    // SAFETY: kCFBooleanTrue / kAXTrustedCheckOptionPrompt 皆為 Apple 公開 ABI 提供的
    // static singleton，跨進程 read-only 共享，任意執行緒讀取安全。kCFBooleanTrue 文件
    // 保證非 NULL；kAXTrustedCheckOptionPrompt 在 macOS 10.9+ 一律存在。
    // 兩者以 unsafe block 包裹是因為 extern static 在 Rust 2024 edition 強制 unsafe 讀取。
    // 避免 `unwrap()`（憲法 §3.1），採 if-let 並在罕見 None 情境降級到不 prompt 的查詢。
    let Some(true_value): Option<&CFBoolean> = (unsafe { kCFBooleanTrue }) else {
        return is_trusted();
    };
    let key: &CFString = unsafe { kAXTrustedCheckOptionPrompt };
    let dict = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[true_value]);
    // SAFETY: dict 為剛建立的有效 CFDictionary（CFRetained 持有所有權，至少存活到本
    // function 結束）；`as_opaque()` 回傳的 reference 與 dict 同生命週期。
    // AXIsProcessTrustedWithOptions 為 Apple public ABI，文件保證任意執行緒可呼叫；
    // 對 Option<&CFDictionary> 接受 Some/None。
    unsafe { AXIsProcessTrustedWithOptions(Some(dict.as_opaque())) }
}

/// 目前前景（focused）app 的 PID。供注入焦點守衛（`raflow_input::FocusGuard`，
/// security audit run-1 Finding 1）使用：printer 在 `SessionStarted` 記基準、每次注入前
/// 比對，PID 變了就停止本 session 的注入，避免文字與 backspace 打進中途切過去的 app。
///
/// 查不到（AX 未授權 / 無 focused app / query 失敗）→ `None`，守衛端 fail-open。
/// AX client C API 文件保證任意執行緒可呼叫，printer thread 直接用。
pub fn frontmost_app_pid() -> Option<i32> {
    if !is_trusted() {
        return None;
    }
    // SAFETY: AXUIElement::new_system_wide 是 wrapper over AXUIElementCreateSystemWide，
    // 該 C API 無前置條件、任意執行緒可呼叫，回傳新 retained CFTypeRef。
    let sys = unsafe { AXUIElement::new_system_wide() };
    let app = copy_ax_element(&sys, "AXFocusedApplication")?;
    let mut pid: i32 = 0;
    // SAFETY: app 為上方取得的有效 CFRetained 引用；pid 為本 stack frame 的 i32
    // （與 Apple `pid_t` ABI 等價），`NonNull::from(&mut pid)` 保證非 null；
    // 任何錯誤回傳值經下方條件過濾，不會使用未寫入的 pid。
    let err = unsafe { app.pid(NonNull::from(&mut pid)) };
    (err == AXError::Success && pid > 0).then_some(pid)
}

/// 偵測目前 focused element 的狀態。三狀態語意請見 module 文件。
pub fn detect_focus() -> FocusDetection {
    if !is_trusted() {
        return FocusDetection::Untrusted;
    }
    let Some(elem) = query_focused_element() else {
        return FocusDetection::Unknown;
    };
    let role = copy_string_attribute(&elem, "AXRole");
    let supports_text_range = is_attr_settable(&elem, "AXSelectedTextRange");
    let editable = classify(role.as_deref(), supports_text_range);
    FocusDetection::Detected(FocusInfo {
        role: role.unwrap_or_else(|| "<unknown>".into()),
        editable,
    })
}

/// 純函式判定：role 命中清單 OR `AXSelectedTextRange` settable → 視為輸入框。
///
/// 抽出此函式以便 TDD（FFI 部分依賴 system focus state，不可重現）。
fn classify(role: Option<&str>, supports_text_range: bool) -> bool {
    if let Some(r) = role {
        if EDITABLE_ROLES.contains(&r) {
            return true;
        }
    }
    supports_text_range
}

/// 拿目前 focused element，雙路 fallback：
/// 1. system-wide → `AXFocusedUIElement`（cover 多數原生 / Safari / 多數 web）
/// 2. system-wide → `AXFocusedApplication` → `AXFocusedUIElement`（cover Electron 系
///    app 像 ChatGPT desktop / Slack / Discord 等——它們對 system-wide query 回 nil
///    但對 app-level query 會吐回實際的 focused web 元件）
fn query_focused_element() -> Option<CFRetained<AXUIElement>> {
    // SAFETY: AXUIElement::new_system_wide 是 wrapper over AXUIElementCreateSystemWide，
    // 該 C API 文件保證任意執行緒可呼叫且回傳新 retained CFTypeRef；不需要前置條件。
    let sys = unsafe { AXUIElement::new_system_wide() };
    if let Some(elem) = copy_ax_element(&sys, "AXFocusedUIElement") {
        return Some(elem);
    }
    let app = copy_ax_element(&sys, "AXFocusedApplication")?;
    copy_ax_element(&app, "AXFocusedUIElement")
}

fn copy_ax_element(el: &AXUIElement, name: &str) -> Option<CFRetained<AXUIElement>> {
    copy_attribute(el, name)?.downcast::<AXUIElement>().ok()
}

fn copy_string_attribute(el: &AXUIElement, name: &str) -> Option<String> {
    let s = copy_attribute(el, name)?.downcast::<CFString>().ok()?;
    Some(s.to_string())
}

/// 取一個 AX attribute 值；無權限 / not-applicable / value-null 一律回 None。
fn copy_attribute(el: &AXUIElement, name: &str) -> Option<CFRetained<CFType>> {
    let attr = CFString::from_str(name);
    let mut out: *const CFType = std::ptr::null();
    let out_ptr = NonNull::new(&mut out)?;
    // SAFETY: el 由 caller 持有有效 CFRetained 引用；attr 為剛建立的 CFString；
    // out_ptr 指向本 stack frame 的 *const CFType slot；AXError 列舉值由 Apple 公開
    // ABI 定義；任何錯誤狀態（NoValue, AttributeUnsupported, APIDisabled, ...）皆走
    // 下方 if 分支回 None，不會觸碰未初始化的 out。
    let err = unsafe { el.copy_attribute_value(&attr, out_ptr) };
    if err != AXError::Success || out.is_null() {
        return None;
    }
    let raw = NonNull::new(out as *mut CFType)?;
    // SAFETY: 上方 AXError::Success 路徑保證 out 是 Apple 給的 +1 retain 計數的
    // CFTypeRef，CFRetained::from_raw 接管所有權；後續 drop 會 CFRelease。
    Some(unsafe { CFRetained::from_raw(raw) })
}

/// 詢問 element 對某 attribute 是否 settable。`AXSelectedTextRange` 對可編輯文字元件
/// 一律回 true，對 button / link / static text 回 AttributeUnsupported（→ false）。
/// AX 屬性 Boolean 型別是 `u8`（1 / 0）。
fn is_attr_settable(el: &AXUIElement, name: &str) -> bool {
    let attr = CFString::from_str(name);
    let mut settable: u8 = 0;
    // SAFETY: el 由 caller 持有有效 CFRetained；attr 為剛建立的 CFString；
    // settable 為本 stack frame 的 u8（與 Apple `Boolean` ABI 等價）；
    // `NonNull::from(&mut settable)` 保證非 null。任何錯誤回傳值經下方 if 過濾。
    let err = unsafe { el.is_attribute_settable(&attr, NonNull::from(&mut settable)) };
    err == AXError::Success && settable != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ensure_trusted_with_prompt()` 至少要能 build + 跑得起來不 panic。系統 TCC
    /// 狀態相依的回傳值在 CI / 本機可能不同，所以不 assert 結果；只驗證 FFI path 是通的
    /// （CFDictionary 建構、kCFBooleanTrue 取用、AXIsProcessTrustedWithOptions 呼叫）。
    #[test]
    fn ensure_trusted_with_prompt_smoke() {
        let _ = ensure_trusted_with_prompt();
    }

    /// `frontmost_app_pid()` 的 FFI path 要能跑不 panic。回傳值依 TCC 狀態與當下
    /// focused app 而異（CI headless 多為 None），只驗證「有值時必為正 PID」的合約。
    #[test]
    fn frontmost_app_pid_smoke() {
        if let Some(pid) = frontmost_app_pid() {
            assert!(pid > 0, "AX 回報的 PID 必為正值，got {pid}");
        }
    }

    /// EDITABLE_ROLES 必須涵蓋常見編輯欄位，且不誤把按鈕／連結／靜態文字當成輸入框。
    #[test]
    fn editable_roles_cover_canonical_macos_text_inputs() {
        assert!(EDITABLE_ROLES.contains(&"AXTextField"));
        assert!(EDITABLE_ROLES.contains(&"AXTextArea"));
        assert!(EDITABLE_ROLES.contains(&"AXSecureTextField"));
        assert!(!EDITABLE_ROLES.contains(&"AXButton"));
        assert!(!EDITABLE_ROLES.contains(&"AXStaticText"));
        assert!(!EDITABLE_ROLES.contains(&"AXLink"));
    }

    /// FocusDetection.suppresses_panel 三狀態語意：
    /// - Untrusted → 抑制 panel（沒權限時 inject 已能用，不要疊 panel 吵）
    /// - Unknown → 不抑制（panel 作安全網）
    /// - Detected(editable=true) → 抑制；Detected(editable=false) → 不抑制
    #[test]
    fn suppresses_panel_decision_table() {
        let cases: &[(FocusDetection, bool, &str)] = &[
            (FocusDetection::Untrusted, true, "untrusted suppresses"),
            (
                FocusDetection::Unknown,
                true,
                "unknown suppresses (Electron fallback)",
            ),
            (
                FocusDetection::Detected(FocusInfo {
                    role: "AXTextField".into(),
                    editable: true,
                }),
                true,
                "detected editable suppresses",
            ),
            (
                FocusDetection::Detected(FocusInfo {
                    role: "AXButton".into(),
                    editable: false,
                }),
                false,
                "detected non-editable does not suppress",
            ),
        ];
        for (det, expected, label) in cases {
            assert_eq!(det.suppresses_panel(), *expected, "{label}");
        }
    }

    /// 參數化覆蓋 `classify` 的決策表：
    /// - role 命中清單 → editable
    /// - AXSelectedTextRange settable → editable（不論 role；fallback 路徑）
    /// - 兩者都不滿足 → not editable
    #[test]
    fn classify_decision_table() {
        let cases: &[(Option<&str>, bool, bool, &str)] = &[
            // (role, supports_text_range, expected_editable, label)
            (Some("AXTextField"), false, true, "TextField by role"),
            (Some("AXTextArea"), false, true, "TextArea by role"),
            (Some("AXSearchField"), false, true, "SearchField by role"),
            (
                Some("AXSecureTextField"),
                false,
                true,
                "SecureTextField by role",
            ),
            (Some("AXComboBox"), false, true, "ComboBox by role"),
            // Chrome lazy AX tree fallback：role 不在清單但 SelectedTextRange settable
            (
                Some("AXGroup"),
                true,
                true,
                "AXGroup with text range fallback",
            ),
            (Some(""), true, true, "empty role with text range fallback"),
            (None, true, true, "no role with text range fallback"),
            (
                Some("AXUnknown"),
                true,
                true,
                "unknown role with text range fallback",
            ),
            // 非輸入框：role 不在清單且 SelectedTextRange 不 settable
            (Some("AXButton"), false, false, "Button no fallback"),
            (Some("AXLink"), false, false, "Link no fallback"),
            (Some("AXStaticText"), false, false, "StaticText no fallback"),
            (Some("AXGroup"), false, false, "Group no fallback"),
            (None, false, false, "no role no fallback"),
        ];
        for (role, supports, expected, label) in cases {
            let got = classify(*role, *supports);
            assert_eq!(
                got, *expected,
                "{label}: role={role:?} text_range={supports} → {got}, want {expected}"
            );
        }
    }
}
