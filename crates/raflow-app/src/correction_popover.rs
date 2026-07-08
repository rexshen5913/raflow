//! D1 詞庫成長：「教 raflow 一個更正」擷取 popover（`docs/design/vocabulary-growth.md` §3）。
//!
//! 用 `NSAlert` + accessory view（`NSComboBox` 聽成 / `NSTextField` 正確 / `NSButton` 勾選
//! 「也加優先區」）+ 同步 `runModal()`——免自訂 Obj-C target-action 類別、同步取值，較 raw
//! target-action 穩健且仍是 NSPanel 家族。另暫裝一個含標準 `cut:/copy:/paste:/selectAll:` 的
//! `NSMenu`/`NSMenuItem` Edit 主選單（menu bar app 無主選單 → 文字欄收不到 Cmd+X/C/V/A），modal
//! 結束後還原。ADR-0005 例外四涵蓋本檔的 unsafe FFI（objc2 0.6 已把多數 AppKit API 標為 safe
//! wrapper，故此處僅 `addItemWithObjectValue` 與 `NSMenuItem::initWithTitle_action_keyEquivalent`
//! 兩處需 `unsafe`，各附 SAFETY 註解）。
//!
//! Threading：只能在主執行緒呼叫；`prompt_correction()` 以 `MainThreadMarker` 強制，非主
//! 執行緒回 `None`。

use objc2::runtime::Sel;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy, NSButton,
    NSButtonType, NSComboBox, NSControlStateValueOn, NSEvent, NSFloatingWindowLevel, NSMenu,
    NSMenuItem, NSModalResponse, NSScreen, NSTextField, NSView, NSWindowCollectionBehavior,
};
use objc2_foundation::{NSPoint, NSPointInRect, NSRect, NSSize, NSString};

/// 在使用者**目前所在的螢幕/Space** 置中顯示 NSAlert 並跑 modal，回傳 response。
///
/// LSUIElement（menu bar 背景程式）的 modal 若不特別處理會有三個問題：
///   (a) 切 `.regular` 前景後只冒出 Dock 圖示、視窗躲在後面 → 要手點 Dock 才浮上來；
///   (b) 出現在 app 上次所在的 Space，而非使用者目前的 Space；
///   (c) 雙螢幕時 NSAlert 預設在**主螢幕**置中，而非使用者正在操作的那個螢幕。
///
/// 對策：`MoveToActiveSpace` + floating level + `orderFrontRegardless` 解 (a)(b)；把視窗重新
/// 置中到「滑鼠游標所在螢幕」解 (c)。並改用 `runModalForWindow`（而非 `alert.runModal()`）——
/// 後者每次會把面板重新置中回主螢幕、蓋掉我們的定位；前者尊重我們設好的 frame。按鈕回傳碼相同
/// （NSAlert 按鈕觸發 `stopModalWithCode:` → `NSAlertFirstButtonReturn` 等）。
///
/// 呼叫端仍需自行暫升 `.regular` + activate（見各 caller）。所有 raflow 的 modal 都應走此函式。
pub fn run_alert_on_active_screen(alert: &NSAlert, mtm: MainThreadMarker) -> NSModalResponse {
    // 先 layout 讓視窗尺寸定案，才能正確算置中原點。
    alert.layout();
    let win = alert.window();
    win.setCollectionBehavior(NSWindowCollectionBehavior::MoveToActiveSpace);
    win.setLevel(NSFloatingWindowLevel);

    // 置中到滑鼠游標所在的螢幕（雙螢幕：你在哪個螢幕操作，就在哪個螢幕跳）。找不到就退回預設位置。
    let cursor = NSEvent::mouseLocation();
    for screen in NSScreen::screens(mtm).iter() {
        if NSPointInRect(cursor, screen.frame()) {
            let vf = screen.visibleFrame();
            let wf = win.frame();
            let origin = NSPoint::new(
                vf.origin.x + (vf.size.width - wf.size.width) / 2.0,
                vf.origin.y + (vf.size.height - wf.size.height) / 2.0,
            );
            win.setFrameOrigin(origin);
            break;
        }
    }

    win.makeKeyAndOrderFront(None);
    win.orderFrontRegardless();
    let response = NSApplication::sharedApplication(mtm).runModalForWindow(&win);
    // `alert.runModal()` 結束時會自動把視窗 order out；手動 `runModalForWindow` 不會，
    // 故按鈕點完 modal 已結束、視窗卻仍留在畫面 → 手動隱藏。
    win.orderOut(None);
    response
}

/// 擷取結果：使用者按「記住」後填的內容。
pub struct CorrectionInput {
    /// 聽成（被聽錯的字；可從最近注入 token 下拉挑，也可自行輸入）。
    pub heard: String,
    /// 正確拼法。
    pub correct: String,
    /// 是否同時加進 Whisper 優先區（`contextual_terms.txt`）。
    pub add_to_priority: bool,
}

const ACCESSORY_WIDTH: f64 = 300.0;
const ROW_H: f64 = 24.0;
const GAP: f64 = 8.0;

/// 顯示更正擷取 popover（模態）。`candidates` 為「最近注入英文 token」候選，填入聽成下拉。
/// 回傳 `Some` = 按「記住」（heard/correct 為當下欄位值，未 trim，交由純核心
/// `upsert_replacement` 處理）；`None` = 取消、非主執行緒、或建立失敗。
pub fn prompt_correction(candidates: &[String]) -> Option<CorrectionInput> {
    let mtm = MainThreadMarker::new()?;

    // accessory view：由下而上排 checkbox → 正確 → 聽成（AppKit 座標原點在左下）。
    let view_h = ROW_H * 3.0 + GAP * 2.0;
    let view = NSView::initWithFrame(
        NSView::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ACCESSORY_WIDTH, view_h)),
    );

    // 聽成：可編輯 NSComboBox（可挑候選也可自行輸入），置最上列。
    let combo = NSComboBox::initWithFrame(
        NSComboBox::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, ROW_H * 2.0 + GAP * 2.0),
            NSSize::new(ACCESSORY_WIDTH, ROW_H),
        ),
    );
    for c in candidates {
        let s = NSString::from_str(c);
        // SAFETY: `addItemWithObjectValue` 取 raw `&AnyObject`（objc2 無法驗證型別），`s` 是
        // NSComboBox 接受的 NSString object value、於呼叫期間存活；主執行緒由 MainThreadMarker 保證。
        unsafe { combo.addItemWithObjectValue(&s) };
    }
    // 預設選第一個候選（最近注入者），沒有候選就留空讓使用者輸入。
    if let Some(first) = candidates.first() {
        combo.setStringValue(&NSString::from_str(first));
    }
    combo.setPlaceholderString(Some(&NSString::from_str("聽成的英文（可下拉挑最近的字）")));

    // 正確：可編輯 NSTextField，中間列。
    let correct_field = NSTextField::initWithFrame(
        NSTextField::alloc(mtm),
        NSRect::new(
            NSPoint::new(0.0, ROW_H + GAP),
            NSSize::new(ACCESSORY_WIDTH, ROW_H),
        ),
    );
    correct_field.setEditable(true);
    correct_field.setSelectable(true);
    correct_field.setBezeled(true);
    correct_field.setPlaceholderString(Some(&NSString::from_str("正確拼法")));

    // 也加優先區：NSButton switch（checkbox），最下列。預設勾選（源頭 priming 通常想要）。
    let checkbox = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(ACCESSORY_WIDTH, ROW_H)),
    );
    checkbox.setButtonType(NSButtonType::Switch);
    checkbox.setTitle(&NSString::from_str("也加進 Whisper 優先區"));
    checkbox.setState(NSControlStateValueOn);

    view.addSubview(&combo);
    view.addSubview(&correct_field);
    view.addSubview(&checkbox);

    // NSAlert 承載 accessory view + 記住/取消 按鈕。
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("教 raflow 一個更正"));
    alert.setInformativeText(&NSString::from_str(
        "把「聽成」的字更正為「正確」拼法，寫入取代規則；下次錄音生效。",
    ));
    alert.addButtonWithTitle(&NSString::from_str("記住"));
    alert.addButtonWithTitle(&NSString::from_str("取消"));
    alert.setAccessoryView(Some(&view));

    // 讓 dialog 能收鍵盤輸入：menu bar accessory（`.accessory`）或未 bundle 的 CLI 若不是前景
    // 正規 app，modal 視窗拿不到 key focus → 打字無效（實測現象）。故**暫時升為 `.regular` 前景 app**
    // 並 activate，modal 結束後還原原本 policy（讓真 app 回到 menu-bar-only）。
    let app = NSApplication::sharedApplication(mtm);
    let prev_policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    // 讓文字欄能用 Cmd+X/C/V/A：menu bar app 沒有應用程式主選單 → 這些編輯快捷鍵的 key equivalent
    // 沒被註冊 → 欄位收不到貼上（實測）。暫裝一個含標準 cut/copy/paste/selectAll 的 Edit 主選單，
    // modal 結束後還原原本主選單。
    let prev_menu = app.mainMenu();
    app.setMainMenu(Some(&build_edit_main_menu(mtm)));

    let response: NSModalResponse = run_alert_on_active_screen(&alert, mtm);

    app.setMainMenu(prev_menu.as_deref());
    app.setActivationPolicy(prev_policy);

    // 第一個按鈕（記住）= NSAlertFirstButtonReturn。
    if response != NSAlertFirstButtonReturn {
        return None;
    }

    Some(CorrectionInput {
        heard: combo.stringValue().to_string(),
        correct: correct_field.stringValue().to_string(),
        add_to_priority: checkbox.state() == NSControlStateValueOn,
    })
}

/// 顯示一個一按鈕的原生提示對話框（模態）。D1 只在**出錯**時用（空欄位／寫入失敗）——成功
/// 走「對話框關掉＝成功」不額外提示。無文字輸入故不需 Edit 主選單；同 `prompt_correction` 暫升
/// `.regular` 前景讓對話框可見可聚焦，結束還原。只能主執行緒呼叫。
pub fn show_notice(title: &str, body: &str) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(body));
    alert.addButtonWithTitle(&NSString::from_str("好"));

    let app = NSApplication::sharedApplication(mtm);
    let prev_policy = app.activationPolicy();
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let _ = run_alert_on_active_screen(&alert, mtm);
    app.setActivationPolicy(prev_policy);
}

/// 建最小 Edit 主選單（剪下/複製/貼上/全選），把標準 responder-chain 選擇器綁到 Cmd 快捷鍵。
/// key equivalent 預設修飾鍵為 Cmd，故 `"v"` = Cmd+V；動作由當前 first responder（欄位的 field
/// editor）處理，不需自訂 target。
fn build_edit_main_menu(mtm: MainThreadMarker) -> objc2::rc::Retained<NSMenu> {
    let edit_menu = NSMenu::new(mtm);
    let add = |title: &str, action: Sel, key: &str| {
        // SAFETY: `initWithTitle_action_keyEquivalent` 為 unsafe（objc2 無法驗證 selector 對
        // responder chain 合法）。這裡的 action 皆為 NSResponder 標準編輯選擇器（cut:/copy:/
        // paste:/selectAll:），title/keyEquivalent 為有效 NSString，主執行緒由 mtm 保證。
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                Some(action),
                &NSString::from_str(key),
            )
        };
        edit_menu.addItem(&item);
    };
    add("剪下", sel!(cut:), "x");
    add("複製", sel!(copy:), "c");
    add("貼上", sel!(paste:), "v");
    add("全選", sel!(selectAll:), "a");

    let edit_item = NSMenuItem::new(mtm);
    edit_item.setSubmenu(Some(&edit_menu));
    let main_menu = NSMenu::new(mtm);
    main_menu.addItem(&edit_item);
    main_menu
}
