//! 依 macOS「目前鍵盤輸入源」自動選 speech locale（每次錄音開始時取樣一次）。
//!
//! 動機：`SFSpeechRecognizer` 是單一 locale 的辨識器，設 `zh-TW` 時英文即時辨識會亂。
//! 使用者打字前本來就會切輸入法，raflow 借用這個訊號：輸入法在中文 → `zh-TW`，
//! 在英文 → `en-US`，讓 Apple 用對的語言模型即時上字。
//!
//! ## 範圍與限制
//!
//! - 這是 **session 級**提示：只在按下 hotkey 開始錄音那一刻讀一次，決定本次 locale。
//!   對「同一句話中英夾雜」無解（Apple 單 locale 的硬限制），那屬另案。
//! - 對應採二元 `zh-TW` / `en-US`；偵測失敗或第三語言 → fallback `zh-TW`。
//!
//! ## unsafe 邊界
//!
//! 讀輸入源需呼叫 Carbon Text Input Source Services（TIS）FFI，屬憲法 §3.3 例外五
//! （見 `docs/adr/0007-unsafe-exception-for-input-source-detection.md`）。所有 `unsafe`
//! 集中在私有 helper 並附 `// SAFETY:`；對外 `current_input_locale()` 完全 safe，且純
//! 對應邏輯抽成 [`locale_for_language`] 以便 TDD。

use objc2_core_foundation::{CFRetained, CFString, CFType};
use std::ffi::c_void;
use std::ptr::NonNull;

/// 偵測失敗、或輸入源語言非中英時的 fallback（維持專案原本預設）。
const DEFAULT_LOCALE: &str = "zh-TW";

/// 純函式：輸入源主要語言碼 → speech locale。抽出以便參數化測試（FFI 部分依賴系統
/// 當下輸入法狀態，不可在單元測試重現）。
///
/// - `zh*`（`zh` / `zh-Hant` / `zh-Hans` …）→ `zh-TW`
/// - `en*`（`en` / `en-US` / `en-GB` …）→ `en-US`
/// - 其他語言 / `None` → fallback `zh-TW`
fn locale_for_language(lang: Option<&str>) -> &'static str {
    match lang.map(|l| l.to_ascii_lowercase()) {
        Some(l) if l.starts_with("zh") => "zh-TW",
        Some(l) if l.starts_with("en") => "en-US",
        _ => DEFAULT_LOCALE,
    }
}

/// 讀取目前鍵盤輸入源，回傳對應的 speech locale（`"zh-TW"` 或 `"en-US"`）。
/// 任何失敗（無輸入源、無語言屬性）都安全降級到 [`DEFAULT_LOCALE`]。
pub fn current_input_locale() -> String {
    let lang = current_input_primary_language();
    locale_for_language(lang.as_deref()).to_string()
}

// Carbon Text Input Source Services。ADR-0007。
// `kTISPropertyInputSourceLanguages` 為 Apple 公開 ABI 的 static CFStringRef 常數。
#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: *const CFString) -> *mut c_void;
    static kTISPropertyInputSourceLanguages: *const CFString;
}

// CoreFoundation：讀 languages 陣列。以原始 C API 呼叫，避免依賴 objc2 CFArray 綁定；
// CoreFoundation framework 由 objc2-core-foundation 連結，重複宣告的 extern 由 linker 去重。
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(arr: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(arr: *const c_void, idx: isize) -> *const c_void;
}

/// 取目前輸入源 languages 屬性的第 0 筆（主要語言碼），例如 `"zh-Hant"` / `"en"`。
fn current_input_primary_language() -> Option<String> {
    // SAFETY: TISCopyCurrentKeyboardInputSource 為 Apple 公開 ABI，無前置條件，回傳
    // 一個 +1 retained 的 TISInputSourceRef（CFTypeRef）或 NULL。
    let src_raw = unsafe { TISCopyCurrentKeyboardInputSource() };
    let src_nn = NonNull::new(src_raw as *mut CFType)?;
    // SAFETY: src_nn 為上面 +1 retained 的有效 CFTypeRef；CFRetained 接管所有權，
    // 於 drop 時 CFRelease，平衡 Copy 語意的 retain。
    let src: CFRetained<CFType> = unsafe { CFRetained::from_raw(src_nn) };

    // SAFETY: kTISPropertyInputSourceLanguages 為 Apple 提供的 static CFStringRef 常數；
    // 讀取 extern static 在 Rust 2024 需 unsafe。
    let key = unsafe { kTISPropertyInputSourceLanguages };
    // SAFETY: src 為上面持有的有效 TISInputSourceRef；key 為有效 CFStringRef。
    // TISGetInputSourceProperty 為 GET 語意，回傳的 CFArrayRef 由 input source 擁有
    // （不可 release），其生命週期涵蓋 src 存活期間；失敗時回 NULL。
    let src_ptr = (&*src as *const CFType) as *mut c_void;
    let arr_raw = unsafe { TISGetInputSourceProperty(src_ptr, key) };
    if arr_raw.is_null() {
        return None;
    }

    // SAFETY: arr_raw 非 NULL 時為有效 CFArrayRef（CFString 元素），存活期由 src 保證。
    let count = unsafe { CFArrayGetCount(arr_raw) };
    if count <= 0 {
        return None;
    }
    // SAFETY: 0 < count，index 0 合法；回傳 array 擁有的 CFStringRef（GET 語意），不可 release。
    let elem = unsafe { CFArrayGetValueAtIndex(arr_raw, 0) };
    let elem_nn = NonNull::new(elem as *mut CFString)?;
    // SAFETY: elem_nn 指向 array 擁有的有效 CFString，存活期涵蓋本借用；以唯讀 &CFString
    // 借用不改變 retain count；to_string 會複製出獨立 Rust String，於 src drop 前完成。
    let s: &CFString = unsafe { elem_nn.as_ref() };
    Some(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 參數化覆蓋 `locale_for_language` 的對應決策表。
    #[test]
    fn locale_for_language_maps_zh_en_with_fallback() {
        let cases: &[(Option<&str>, &str, &str)] = &[
            // 中文各變體 → zh-TW
            (Some("zh"), "zh-TW", "bare zh"),
            (Some("zh-Hant"), "zh-TW", "traditional"),
            (Some("zh-Hans"), "zh-TW", "simplified"),
            (Some("zh-TW"), "zh-TW", "already zh-TW"),
            (Some("ZH-HANT"), "zh-TW", "uppercase normalised"),
            // 英文各變體 → en-US
            (Some("en"), "en-US", "bare en"),
            (Some("en-US"), "en-US", "en-US"),
            (Some("en-GB"), "en-US", "en-GB folds to en-US"),
            (Some("EN"), "en-US", "uppercase en"),
            // 第三語言 / 空 / None → fallback zh-TW
            (Some("ja"), "zh-TW", "japanese falls back"),
            (Some("ko"), "zh-TW", "korean falls back"),
            (Some(""), "zh-TW", "empty falls back"),
            (None, "zh-TW", "none falls back"),
        ];
        for (lang, expected, label) in cases {
            assert_eq!(
                locale_for_language(*lang),
                *expected,
                "{label}: lang={lang:?} → want {expected}"
            );
        }
    }

    /// FFI 冒煙測試：`current_input_locale()` 必須跑得起來不 panic，且回傳落在支援集合。
    /// 實際值相依系統當下輸入法，故不 assert 特定值。
    #[test]
    fn current_input_locale_returns_supported_locale() {
        let locale = current_input_locale();
        assert!(
            locale == "zh-TW" || locale == "en-US",
            "unexpected locale: {locale:?}"
        );
    }
}
