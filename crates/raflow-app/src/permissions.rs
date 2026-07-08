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
//!   - NSAlert 引導 / 診斷（`show_onboarding` / `show_diagnostics`）：objc2-app-kit 既有 safe API（無 unsafe）。
//!
//! 模組入口（`main.rs`）已用 `#[cfg(target_os = "macos")]` 限定，這裡不再重複。

use std::path::PathBuf;
use std::process::Command;

/// raflow 首次啟動需即時查詢並引導的三道權限（`app.md §9.2`）。`ALL` 的順序即引導顯示順序。
///
/// Input Monitoring（雙擊 Cmd 偵測）**不**在此查詢：raflow 的雙擊偵測與文字注入都走**輔助使用**
/// （NSEvent 全域監看 + enigo 皆以 Accessibility 為 gate），而 macOS 對已授權輔助使用的 app 會讓
/// 其涵蓋 listen-event 存取（`IOHIDCheckAccess` 回 Granted），raflow 從不獨立出現在 Input Monitoring
/// 清單。故單獨查詢它對 raflow 無診斷價值（永遠跟著輔助使用走），只在引導文末以文字提示。
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
         改完可從 menu bar 圖示 →「診斷…」再次確認剩下幾項。",
    );
    // 輔助使用特別：enigo 在啟動時就快取了授權狀態，執行中才授權**不會生效**，必須重啟 raflow。
    // 這是 macOS 對 Accessibility 的行為，其他權限不受影響。
    if missing.contains(&Permission::Accessibility) {
        body.push_str(
            "\n\n⚠ 「輔助使用」授權後，請從 menu →「重新啟動 raflow」重啟一次才會生效\
             （其他權限即時生效、不用重啟）。",
        );
    }
    body.push_str(
        "\n\n（若雙擊 Cmd 完全沒反應，同樣是「輔助使用」未授權——raflow 的雙擊偵測以它為準，\
         不需另外開「輸入監控」。）",
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

// ── 診斷讀表（menu「診斷…」：每項即時狀態）──────────────────────────────────────────
//
// 只涵蓋 raflow 實際使用的資源：三道權限（麥克風／語音辨識／輔助使用）+ Whisper／VAD 模型檔。
// **不含 Input Monitoring**：raflow 的雙擊偵測與注入都走輔助使用，輸入監控被其涵蓋、對 raflow
// 無獨立診斷價值（詳見 `Permission` 上方註解）。

/// 診斷讀表單列的嚴重度，決定顯示圖示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagLevel {
    /// 已就緒（✅）。
    Ok,
    /// 有問題、影響功能（⚠️）。
    Warn,
}

impl DiagLevel {
    /// 讀表前綴圖示。
    pub fn icon(self) -> &'static str {
        match self {
            DiagLevel::Ok => "✅",
            DiagLevel::Warn => "⚠️",
        }
    }
}

/// 診斷讀表的一列：標籤 + 嚴重度 + 一句話狀態／後果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagRow {
    pub label: &'static str,
    pub level: DiagLevel,
    pub detail: String,
}

/// 建構診斷讀表所需的即時輸入。抽成 struct 讓 [`build_diagnostic`] 成為可參數化測試的純函式
/// （檔案系統查詢集中在 [`collect_diag_inputs`]）。
#[derive(Debug, Clone, Copy)]
pub struct DiagInputs {
    pub microphone: bool,
    pub speech_recognition: bool,
    pub accessibility: bool,
    /// 輔助使用已授權、但 enigo 已快取啟動時的未授權狀態 → 需重啟才生效（見 `onboarding_body`）。
    /// 由 `main.rs`（唯一知道 `launched_ax_trusted` 的地方）注入。
    pub accessibility_needs_restart: bool,
    pub whisper_model_ready: bool,
    pub vad_model_ready: bool,
}

/// 純函式：即時輸入 → 五列診斷讀表。順序固定：麥克風 → 語音辨識 → 輔助使用 →
/// Whisper 模型 → VAD 模型。抽出以便 TDD（憲法 §2）。
pub fn build_diagnostic(inputs: &DiagInputs) -> Vec<DiagRow> {
    let row = |label, level, detail: &str| DiagRow {
        label,
        level,
        detail: detail.to_string(),
    };
    let ok_or_warn = |granted, label, ok: &str, warn: &str| {
        if granted {
            row(label, DiagLevel::Ok, ok)
        } else {
            row(label, DiagLevel::Warn, warn)
        }
    };

    // 輔助使用三態：未授權 / 已授權待重啟 / 已授權就緒。
    let accessibility = if !inputs.accessibility {
        row("輔助使用", DiagLevel::Warn, "未授權——只能手動 Cmd+V 貼上")
    } else if inputs.accessibility_needs_restart {
        row(
            "輔助使用",
            DiagLevel::Warn,
            "已授權，但需重新啟動 raflow 才生效（menu →「重新啟動 raflow」）",
        )
    } else {
        row("輔助使用", DiagLevel::Ok, "已授權")
    };

    vec![
        ok_or_warn(
            inputs.microphone,
            "麥克風",
            "已授權",
            "未授權——說話錄到靜音、完全沒有字",
        ),
        ok_or_warn(
            inputs.speech_recognition,
            "語音辨識",
            "已授權",
            "未授權——無法把語音轉成文字",
        ),
        accessibility,
        ok_or_warn(
            inputs.whisper_model_ready,
            "Whisper 模型",
            "已就緒",
            "缺檔——終校停用，回退 Apple 即時輸出",
        ),
        ok_or_warn(
            inputs.vad_model_ready,
            "VAD 模型",
            "已就緒",
            "缺檔——退化為整段校正（非句級滾動）",
        ),
    ]
}

/// 純函式：讀表中「可由開啟系統設定解決」的**第一個**權限缺項的設定深連結。
/// 只涵蓋三道權限（模型缺檔開設定沒用，需重新下載，不列入）；全部就緒 → `None`（無需設定按鈕）。
/// 順序與讀表一致：麥克風 → 語音辨識 → 輔助使用。
pub fn first_actionable_settings_url(inputs: &DiagInputs) -> Option<&'static str> {
    if !inputs.microphone {
        Some(Permission::Microphone.settings_url())
    } else if !inputs.speech_recognition {
        Some(Permission::SpeechRecognition.settings_url())
    } else if !inputs.accessibility {
        Some(Permission::Accessibility.settings_url())
    } else {
        None
    }
}

/// 即時蒐集診斷輸入：三道權限快照 + Whisper／VAD 模型檔存在與否。
/// `accessibility_needs_restart` 由 caller（`main.rs`）注入，這裡無從得知 `launched_ax_trusted`。
pub fn collect_diag_inputs(accessibility_needs_restart: bool) -> DiagInputs {
    let snap = capture_snapshot();
    DiagInputs {
        microphone: snap.microphone,
        speech_recognition: snap.speech_recognition,
        accessibility: snap.accessibility,
        accessibility_needs_restart: snap.accessibility && accessibility_needs_restart,
        whisper_model_ready: model_file_ready(raflow_speech::resolve_model_path()),
        vad_model_ready: model_file_ready(raflow_speech::resolve_vad_model_path()),
    }
}

/// 模型路徑指向**實體檔案** → 就緒。路徑解析失敗（`None`）、不存在、或指向目錄 → 未就緒。
/// 用 `is_file()`（非 `exists()`）避免把同名目錄誤判為模型就緒。
fn model_file_ready(path: Option<PathBuf>) -> bool {
    path.as_deref().is_some_and(|p| p.is_file())
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
/// 缺項的設定頁（其餘缺項於改完後由「診斷…」再逐一導引）。`missing` 為空時不彈窗。
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
            // 直接開設定頁即可——啟動時 `register_silently()`（prompt:false）已把 raflow 註冊進
            // 「輔助使用」清單（實機驗證：清單中確有 raflow），故不需再跳系統 AX 框。
            open_settings(first.settings_url());
        }
    }
}

/// menu「診斷…」的診斷讀表：列出五項（麥克風／語音辨識／輔助使用／Whisper 模型／VAD 模型）的
/// 即時狀態。有可由系統設定解決的權限缺項時，主按鈕「開啟系統設定」直達**第一個**缺項；
/// 否則單一「好」按鈕。`accessibility_needs_restart` 由 caller 注入（見 [`collect_diag_inputs`]）。
///
/// 只能在主執行緒呼叫（`MainThreadMarker` 強制，非主執行緒安靜跳過）。
pub fn show_diagnostics(accessibility_needs_restart: bool) {
    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy, NSColor,
        NSFont, NSGridCellPlacement, NSGridView, NSTextField, NSView,
    };
    use objc2_foundation::{NSArray, NSString};

    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let inputs = collect_diag_inputs(accessibility_needs_restart);
    let rows = build_diagnostic(&inputs);
    let settings_url = first_actionable_settings_url(&inputs);

    // 三欄讀表用 **NSGridView** 排版：圖示 / 項目 / 狀態說明，各欄由 AppKit 原生對齊。
    // 不再靠等寬字型 + 空白補齊——`monospacedSystemFont` 只保證 ASCII 等寬，CJK 走 fallback
    // 字型、advance 非 ASCII 兩倍，補齊必歪（實機打臉，見 implement.md §18.10）。
    let font = NSFont::systemFontOfSize(13.0);
    let warn_color = NSColor::systemRedColor();
    let detail_color = NSColor::secondaryLabelColor();
    let make_cell = |text: &str, color: &NSColor| -> Retained<NSView> {
        let tf = NSTextField::labelWithString(&NSString::from_str(text), mtm);
        tf.setFont(Some(&font));
        tf.setTextColor(Some(color));
        // NSTextField → NSControl → NSView，放進 NSArray<NSView>。
        Retained::into_super(Retained::into_super(tf))
    };
    let label_color = NSColor::labelColor();
    let grid_rows: Vec<Retained<NSArray<NSView>>> = rows
        .iter()
        .map(|r| {
            let detail_c = if r.level == DiagLevel::Warn {
                &*warn_color
            } else {
                &*detail_color
            };
            let cells = [
                make_cell(r.level.icon(), &label_color),
                make_cell(r.label, &label_color),
                make_cell(&r.detail, detail_c),
            ];
            NSArray::from_retained_slice(&cells)
        })
        .collect();
    let grid = NSGridView::gridViewWithViews(&NSArray::from_retained_slice(&grid_rows), mtm);
    grid.setColumnSpacing(10.0);
    grid.setRowSpacing(7.0);
    grid.columnAtIndex(0).setXPlacement(NSGridCellPlacement::Center); // 圖示置中
    grid.columnAtIndex(1).setXPlacement(NSGridCellPlacement::Leading); // 項目靠左
    grid.columnAtIndex(2).setXPlacement(NSGridCellPlacement::Leading); // 狀態靠左
    // 依內容自動求最適尺寸（NSResponder + NSView 已啟用 → fittingSize 可用）。
    let size = grid.fittingSize();
    grid.setFrameSize(size);

    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("raflow 診斷"));
    alert.setInformativeText(&NSString::from_str("各項即時狀態："));
    alert.setAccessoryView(Some(&grid));
    if settings_url.is_some() {
        alert.addButtonWithTitle(&NSString::from_str("開啟系統設定"));
        alert.addButtonWithTitle(&NSString::from_str("關閉"));
    } else {
        alert.addButtonWithTitle(&NSString::from_str("好"));
    }

    // 暫升 `.regular` 前景讓 LSUIElement 背景程式的對話框可見可聚焦，結束還原（同 show_onboarding）。
    let app = NSApplication::sharedApplication(mtm);
    let prev_policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let response = crate::correction_popover::run_alert_on_active_screen(&alert, mtm);
    app.setActivationPolicy(prev_policy);

    if let Some(url) = settings_url {
        if response == NSAlertFirstButtonReturn {
            open_settings(url);
        }
    }
}

/// 「輔助使用剛授權、但 enigo 已快取啟動時的未授權狀態」時的重啟提議。回傳使用者是否選擇立即重啟。
/// 只能主執行緒呼叫（`MainThreadMarker` 強制，非主執行緒安靜跳過回 false）。
pub fn show_restart_offer() -> bool {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy,
    };
    use objc2_foundation::NSString;

    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("需要重新啟動 raflow"));
    alert.setInformativeText(&NSString::from_str(
        "「輔助使用」已授權，但要**重新啟動 raflow** 才會生效（macOS 對 Accessibility 的行為：\
         啟動後才授權不會即時套用）。重啟前文字仍可用 Cmd+V 從剪貼簿貼上。",
    ));
    alert.addButtonWithTitle(&NSString::from_str("立即重新啟動"));
    alert.addButtonWithTitle(&NSString::from_str("稍後"));

    let app = NSApplication::sharedApplication(mtm);
    let prev_policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let response = crate::correction_popover::run_alert_on_active_screen(&alert, mtm);
    app.setActivationPolicy(prev_policy);

    response == NSAlertFirstButtonReturn
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
    fn onboarding_body_restart_note_only_when_accessibility_missing() {
        // 缺輔助使用 → 內文含「重新啟動」提示（enigo 快取，授權後需重啟才生效）。
        let with_ax = onboarding_body(&[Permission::Accessibility]).unwrap_or_default();
        assert!(with_ax.contains("重新啟動 raflow"), "缺輔助使用應提示重啟");
        // 只缺麥克風 → 不該有重啟提示（其他權限即時生效）。
        let without_ax = onboarding_body(&[Permission::Microphone]).unwrap_or_default();
        assert!(
            !without_ax.contains("重新啟動 raflow"),
            "非輔助使用缺項不該提示重啟"
        );
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
        // 編號從 1 起。
        assert!(body.contains("1."));
        assert!(body.contains("2."));
        // 「雙擊沒反應」的提示應導向輔助使用，不再提 Input Monitoring（實機驗證雙擊 gate 是輔助使用）。
        assert!(body.contains("雙擊 Cmd"));
        assert!(!body.contains("Input Monitoring"));
    }

    fn diag(mic: bool, speech: bool, ax: bool, ax_restart: bool, whisper: bool, vad: bool) -> DiagInputs {
        DiagInputs {
            microphone: mic,
            speech_recognition: speech,
            accessibility: ax,
            accessibility_needs_restart: ax_restart,
            whisper_model_ready: whisper,
            vad_model_ready: vad,
        }
    }

    #[test]
    fn build_diagnostic_has_five_rows_in_fixed_order() {
        let rows = build_diagnostic(&diag(true, true, true, false, true, true));
        let labels: Vec<&str> = rows.iter().map(|r| r.label).collect();
        assert_eq!(
            labels,
            vec!["麥克風", "語音辨識", "輔助使用", "Whisper 模型", "VAD 模型"]
        );
        // 全就緒 → 每列都是 Ok。
        assert!(rows.iter().all(|r| r.level == DiagLevel::Ok));
    }

    #[test]
    fn build_diagnostic_levels_reflect_state() {
        // 全缺（權限缺、模型缺）→ 每列 Warn。
        let rows = build_diagnostic(&diag(false, false, false, false, false, false));
        assert!(rows.iter().all(|r| r.level == DiagLevel::Warn));
        // 麥克風缺項要帶症狀文字。
        assert!(rows[0].detail.contains("靜音"));
    }

    #[test]
    fn accessibility_needs_restart_shows_warn_with_restart_hint() {
        // 已授權但待重啟 → Warn + 重啟提示。
        let rows = build_diagnostic(&diag(true, true, true, true, true, true));
        let ax = rows.iter().find(|r| r.label == "輔助使用").expect("有輔助使用列");
        assert_eq!(ax.level, DiagLevel::Warn);
        assert!(ax.detail.contains("重新啟動"));
        // 未授權（needs_restart 無意義）→ Warn 但不是重啟提示、而是「只能手動」。
        let rows2 = build_diagnostic(&diag(true, true, false, false, true, true));
        let ax2 = rows2.iter().find(|r| r.label == "輔助使用").expect("有輔助使用列");
        assert_eq!(ax2.level, DiagLevel::Warn);
        assert!(ax2.detail.contains("Cmd+V"));
    }

    #[test]
    fn first_actionable_settings_url_follows_priority_and_skips_models() {
        // 全綠 → None（不需要設定按鈕）。
        assert_eq!(
            first_actionable_settings_url(&diag(true, true, true, false, true, true)),
            None
        );
        // 只有模型缺 → 仍 None（開設定沒用，需重新下載）。
        assert_eq!(
            first_actionable_settings_url(&diag(true, true, true, false, false, false)),
            None
        );
        // 麥克風優先於後面的缺項。
        assert_eq!(
            first_actionable_settings_url(&diag(false, false, false, false, true, true)),
            Some(Permission::Microphone.settings_url())
        );
        // 只有輔助使用缺 → 指向輔助使用深連結。
        assert_eq!(
            first_actionable_settings_url(&diag(true, true, false, false, true, true)),
            Some(Permission::Accessibility.settings_url())
        );
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
