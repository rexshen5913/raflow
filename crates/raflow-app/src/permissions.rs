//! 首次啟動的權限引導（onboarding）。
//!
//! 背景：多位使用者回報「雙擊 Cmd 有觸發錄音，但說話完全沒有任何字」。根因是**麥克風**權限
//! 被拒——`raflow-audio`（cpal）在麥克風被拒時仍回 `Ok`、只餵靜音，整條路徑不回錯誤（silent
//! failure）。加上 TCC 以 bundle id 記住拒絕決定，「移除重裝」也無法重置。詳見 ADR-0008。
//!
//! 本模組把原本只有 stderr（Finder 啟動看不到）的權限提示，升級為**看得見的原生 NSAlert**，
//! 並主動查詢/請求三道權限（`app.md §9.2`）：麥克風、語音辨識、輔助使用。
//!
//! 分層（憲法 §1.3 / §2）：
//!   - 純邏輯（`Permission` / `PermissionSnapshot` / `onboarding_body`）：safe，參數化 unit test。
//!   - FFI 查詢（`microphone_granted` / `request_microphone`）：AVFoundation，unsafe 集中（ADR-0008）。
//!   - NSAlert 引導（`show_onboarding` / `show_all_granted`）：objc2-app-kit 既有 safe API（無 unsafe）。
//!
//! 模組入口（`main.rs`）已用 `#[cfg(target_os = "macos")]` 限定，這裡不再重複。

use std::process::Command;

/// raflow 首次啟動需即時查詢並引導的三道權限（`app.md §9.2`）。`ALL` 的順序即引導顯示順序。
///
/// Input Monitoring（雙擊 Cmd 偵測）**不**在此查詢：其失效＝雙擊完全沒反應，本身即顯而易見，
/// 與 silent-mic 陷阱不同；即時查詢需另一組 IOKit/CoreGraphics FFI 與另一個 ADR（見 ADR-0008
/// §1 範圍界定），依 YAGNI 只在引導文末以文字提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Microphone,
    SpeechRecognition,
    Accessibility,
}

impl Permission {
    /// 權限在「系統設定 → 隱私權與安全性」中的顯示名稱。
    pub fn title(self) -> &'static str {
        match self {
            Permission::Microphone => "麥克風",
            Permission::SpeechRecognition => "語音辨識",
            Permission::Accessibility => "輔助使用",
        }
    }

    /// 沒開會怎樣——一句話讓使用者秒懂為什麼要授權（對應各自的實際失效症狀）。
    pub fn symptom(self) -> &'static str {
        match self {
            Permission::Microphone => "收音；沒開會錄到靜音、說話完全沒有任何字",
            Permission::SpeechRecognition => "把語音即時轉成文字；沒開無法辨識",
            Permission::Accessibility => "自動把文字打進輸入框；沒開只能手動 Cmd+V 貼上",
        }
    }

    /// 一鍵直達的系統設定深連結（供 `open` 開啟）。
    pub fn settings_url(self) -> &'static str {
        match self {
            Permission::Microphone => {
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone"
            }
            Permission::SpeechRecognition => {
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_SpeechRecognition"
            }
            Permission::Accessibility => {
                "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility"
            }
        }
    }
}

/// 固定走訪順序，供 [`PermissionSnapshot::missing`] 使用。
const ALL: [Permission; 3] = [
    Permission::Microphone,
    Permission::SpeechRecognition,
    Permission::Accessibility,
];

/// 三道權限的授權快照（`true` = 已授權）。由 [`capture_snapshot`] 即時查詢組成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionSnapshot {
    pub microphone: bool,
    pub speech_recognition: bool,
    pub accessibility: bool,
}

impl PermissionSnapshot {
    fn granted(&self, p: Permission) -> bool {
        match p {
            Permission::Microphone => self.microphone,
            Permission::SpeechRecognition => self.speech_recognition,
            Permission::Accessibility => self.accessibility,
        }
    }

    /// 依 [`ALL`] 的固定順序回傳尚未授權的權限。全綠時為空 vec（呼叫端以 `is_empty()` 判斷全綠）。
    pub fn missing(&self) -> Vec<Permission> {
        ALL.into_iter().filter(|&p| !self.granted(p)).collect()
    }
}

/// 產生引導對話框的內文。`missing` 為空 → `None`（呼叫端據此判斷不需彈窗，避免全綠時打擾）。
pub fn onboarding_body(missing: &[Permission]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let mut body = String::from("raflow 還需要以下權限才能正常運作：\n");
    for (i, p) in missing.iter().enumerate() {
        body.push_str(&format!("\n{}. {}——{}", i + 1, p.title(), p.symptom()));
    }
    body.push_str(
        "\n\n點「開啟系統設定」會直接跳到第一項的設定頁；在清單裡把 raflow 打勾即可。\n\
         改完可從 menu bar 圖示 →「權限檢查…」再次確認剩下幾項。\n\n\
         （若雙擊 Cmd 完全沒反應，是「輸入監控 Input Monitoring」未授權，同樣在\
         「隱私權與安全性」裡開啟。）",
    );
    Some(body)
}

/// 即時查詢三道權限，組成快照。皆**不跳 prompt**：
///   - 麥克風：AVFoundation `AVCaptureDevice`（本模組，ADR-0008）
///   - 語音辨識：`raflow_speech::authorization_granted()`
///   - 輔助使用：`accessibility::is_trusted()`
pub fn capture_snapshot() -> PermissionSnapshot {
    PermissionSnapshot {
        microphone: microphone_granted(),
        speech_recognition: raflow_speech::authorization_granted(),
        accessibility: crate::accessibility::is_trusted(),
    }
}

// ── 麥克風授權 FFI（AVFoundation，ADR-0008）─────────────────────────────────────

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

/// 目前是否已取得麥克風授權（`Authorized`）。**不跳 prompt**，可任意執行緒同步呼叫。
/// 未定 / 被拒 / 受限，或取不到 audio media type，一律回 `false`（引導視為「待授權」）。
pub fn microphone_granted() -> bool {
    // SAFETY: AVMediaTypeAudio 為 AVFoundation extern static（`Option<&'static AVMediaType>`），
    // 讀取無前置條件；None（罕見，理論上不會發生）安全降級為未授權。
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return false;
    };
    // SAFETY: authorizationStatusForMediaType 為 Apple 靜態方法，接受有效 &AVMediaType，
    // 任意執行緒可呼叫且不觸發 prompt。
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    status == AVAuthorizationStatus::Authorized
}

/// 主動請求麥克風授權，回傳最終是否授權。狀態為 `NotDetermined` 時跳系統 prompt；已授權立即回
/// `true`、已拒絕/受限立即回 `false`（Apple 不會二次彈窗）。
///
/// 於首次啟動引導時呼叫——讓麥克風 prompt 發生在引導當下，而非首次聽寫途中才被 cpal 惰性觸發、
/// 且被拒還靜默無聲。
pub async fn request_microphone() -> bool {
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    // SAFETY: 同 microphone_granted 的 SAFETY；讀取 extern static。
    let Some(media_type) = (unsafe { AVMediaTypeAudio }) else {
        return false;
    };

    let (tx, rx) = oneshot::channel::<bool>();
    let tx = Mutex::new(Some(tx));
    let handler = RcBlock::new(move |granted: Bool| {
        if let Ok(mut guard) = tx.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(granted.as_bool());
            }
        }
    });

    // SAFETY: requestAccessForMediaType_completionHandler 為 Apple 靜態方法，接受有效 &AVMediaType
    // 與 completion block；handler 以 RcBlock 保活至本 async fn 結束（await 之後），Apple 於使用者
    // 回應時在內部執行緒呼叫之。
    unsafe {
        AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler);
    }

    // handler 被 drop（例如 process 提前結束）時 sender 消失 → 安全降級為未授權。
    rx.await.unwrap_or(false)
}

// ── 引導對話框（NSAlert，objc2-app-kit safe API，無 unsafe）──────────────────────

/// 顯示首次啟動 / menu 觸發的權限引導。列出所有缺項與其用途；主按鈕「開啟系統設定」直達**第一個**
/// 缺項的設定頁（其餘缺項於改完後由「權限檢查…」再逐一導引）。`missing` 為空時不彈窗。
///
/// 只能在主執行緒呼叫（`MainThreadMarker` 強制，非主執行緒安靜跳過）。
pub fn show_onboarding(missing: &[Permission]) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    };
    use objc2_foundation::NSString;

    let Some(body) = onboarding_body(missing) else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("raflow 需要一些權限"));
    alert.setInformativeText(&NSString::from_str(&body));
    alert.addButtonWithTitle(&NSString::from_str("開啟系統設定"));
    alert.addButtonWithTitle(&NSString::from_str("稍後"));

    // 暫升 `.regular` 前景，讓 LSUIElement 背景程式的對話框可見可聚焦，結束還原原本 policy。
    let app = NSApplication::sharedApplication(mtm);
    let prev_policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    // 置中到游標所在螢幕/Space 並跑 modal（共用 helper，解 Dock 手點 / 跑錯 Space / 跑錯螢幕）。
    let response = crate::correction_popover::run_alert_on_active_screen(&alert, mtm);
    app.setActivationPolicy(prev_policy);

    if response == NSAlertFirstButtonReturn {
        if let Some(first) = missing.first() {
            open_settings(first.settings_url());
        }
    }
}

/// menu「權限檢查…」在三道權限全綠時給的確認提示。
pub fn show_all_granted() {
    crate::correction_popover::show_notice(
        "權限都已就緒",
        "麥克風、語音辨識、輔助使用都已授權，raflow 可以正常聽寫。",
    );
}

/// 以 macOS `open` 開啟系統設定深連結。失敗只記 log（引導本身已顯示，非致命）。
fn open_settings(url: &str) {
    if let Err(e) = Command::new("open").arg(url).spawn() {
        eprintln!("raflow: 無法開啟系統設定（{url}）：{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mic: bool, speech: bool, ax: bool) -> PermissionSnapshot {
        PermissionSnapshot {
            microphone: mic,
            speech_recognition: speech,
            accessibility: ax,
        }
    }

    #[test]
    fn missing_reports_ungranted_in_fixed_order() {
        // (snapshot, 期望缺項)：涵蓋全綠、全缺、各種部分缺；順序恆為 Mic → Speech → Accessibility。
        let cases: Vec<(PermissionSnapshot, Vec<Permission>)> = vec![
            (snap(true, true, true), vec![]),
            (
                snap(false, false, false),
                vec![
                    Permission::Microphone,
                    Permission::SpeechRecognition,
                    Permission::Accessibility,
                ],
            ),
            (snap(false, true, true), vec![Permission::Microphone]),
            (snap(true, false, true), vec![Permission::SpeechRecognition]),
            (snap(true, true, false), vec![Permission::Accessibility]),
            (
                snap(true, false, false),
                vec![Permission::SpeechRecognition, Permission::Accessibility],
            ),
            (
                snap(false, true, false),
                vec![Permission::Microphone, Permission::Accessibility],
            ),
        ];
        for (snapshot, expected) in cases {
            assert_eq!(snapshot.missing(), expected, "snapshot={snapshot:?}");
            assert_eq!(
                snapshot.missing().is_empty(),
                expected.is_empty(),
                "snapshot={snapshot:?}"
            );
        }
    }

    #[test]
    fn onboarding_body_none_when_all_granted() {
        assert!(onboarding_body(&[]).is_none());
        assert!(snap(true, true, true).missing().is_empty());
    }

    #[test]
    fn onboarding_body_lists_each_missing_permission_with_symptom() {
        let missing = vec![Permission::Microphone, Permission::Accessibility];
        let Some(body) = onboarding_body(&missing) else {
            panic!("有缺項應產生內文");
        };
        // 每個缺項的名稱與症狀都要出現，且未列入的語音辨識不該出現。
        assert!(body.contains("麥克風"));
        assert!(body.contains(Permission::Microphone.symptom()));
        assert!(body.contains("輔助使用"));
        assert!(body.contains(Permission::Accessibility.symptom()));
        assert!(!body.contains("語音辨識"));
        // 編號從 1 起、含 Input Monitoring 的文字提示。
        assert!(body.contains("1."));
        assert!(body.contains("2."));
        assert!(body.contains("Input Monitoring"));
    }

    #[test]
    fn settings_urls_target_correct_privacy_panes() {
        let cases = [
            (Permission::Microphone, "Privacy_Microphone"),
            (Permission::SpeechRecognition, "Privacy_SpeechRecognition"),
            (Permission::Accessibility, "Privacy_Accessibility"),
        ];
        for (perm, pane) in cases {
            assert!(
                perm.settings_url().ends_with(pane),
                "{perm:?} 深連結應指向 {pane}"
            );
            assert!(
                perm.settings_url()
                    .starts_with("x-apple.systempreferences:")
            );
        }
    }
}
