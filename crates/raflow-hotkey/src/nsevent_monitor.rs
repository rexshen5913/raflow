//! NSEvent global monitor 接線（ADR-0004 unsafe 例外的唯一棲身處）。
//!
//! 流程：
//!   - `register()` 於主執行緒被呼叫一次，建立 NSEvent 監聽，註冊 keyDown / keyUp /
//!     flagsChanged 三種 mask
//!   - callback 由 macOS 主 run loop 派送，內部維護「Cmd 上一狀態」+ `DoubleTapDetector`
//!   - 偵測到雙擊 → `tx.send(HotkeyEvent::Pressed)`
//!   - `HotkeyHandle` drop → `NSEvent::removeMonitor`

use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use raflow_core::{HotkeyEvent, RaflowError};
use tokio::sync::mpsc::UnboundedSender;

use objc2_core_graphics::{CGEvent, CGEventField};

use crate::activity::key_is_user_takeover;
use crate::double_tap::DoubleTapDetector;

/// 雙擊兩次 Cmd 按下之間的最大時間差。
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(300);

/// 監聽器的擁有者；drop 時呼叫 `NSEvent::removeMonitor` 解除。
pub struct HotkeyHandle {
    monitor: Option<Retained<AnyObject>>,
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        if let Some(token) = self.monitor.take() {
            // SAFETY: removeMonitor 接受先前由 addGlobalMonitorForEventsMatchingMask_handler
            // 回傳的同一個 token；我們持有唯一 strong ref，drop 時消耗註冊。
            unsafe {
                NSEvent::removeMonitor(&token);
            }
        }
    }
}

/// 註冊雙擊 Cmd 偵測。`on_toggle` 於**每次雙擊命中時、在主執行緒**（NSEvent global monitor 的
/// callback 由主 run loop 派送）被呼叫一次，緊接在送出 [`HotkeyEvent::Pressed`] 之前——供呼叫端
/// 在**主執行緒**取樣需要主執行緒的狀態（例如 Carbon TIS 讀當前輸入法選 speech locale；見 ADR-0007，
/// 避免 worker thread 跨執行緒呼叫 TIS 觸發 `dispatch_assert_queue` 崩潰）。
pub fn register<F: Fn() + 'static>(
    tx: UnboundedSender<HotkeyEvent>,
    on_toggle: F,
) -> Result<HotkeyHandle, RaflowError> {
    // 確認主執行緒。NSEvent global monitor 的 callback 由主 run loop 派送，
    // 註冊呼叫亦應於主執行緒；非主執行緒呼叫即為使用者錯誤。
    let _mtm = MainThreadMarker::new().ok_or_else(|| RaflowError::HotkeyRegister {
        detail: "register() must be called from the main thread".into(),
    })?;

    let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;

    // closure 捕獲：tx + 雙擊狀態機 + Cmd 上一刻狀態（用 Mutex 包以滿足 Fn）。
    let detector = Mutex::new(DoubleTapDetector::new(DOUBLE_TAP_WINDOW));
    let last_cmd = Mutex::new(false);

    let handler: RcBlock<dyn Fn(NonNull<NSEvent>)> =
        RcBlock::new(move |event_ptr: NonNull<NSEvent>| {
            // SAFETY: Apple 對 addGlobalMonitorForEvents 的 handler block 契約保證
            // event 參數在 callback 期間為有效非空 NSEvent。
            let event: &NSEvent = unsafe { event_ptr.as_ref() };
            let event_type = event.r#type();
            let now = Instant::now();

            match event_type {
                NSEventType::FlagsChanged => {
                    let flags = event.modifierFlags();
                    let cmd_now = flags.contains(NSEventModifierFlags::Command);

                    let Ok(mut cmd_was) = last_cmd.lock() else {
                        return;
                    };
                    let Ok(mut det) = detector.lock() else {
                        return;
                    };

                    if cmd_now != *cmd_was {
                        if cmd_now {
                            if det.on_cmd_down(now) {
                                // 雙擊命中：此 callback 於**主執行緒**執行，先讓呼叫端在主執行緒
                                // 取樣（如 TIS 讀輸入法，ADR-0007），再送事件給 worker。
                                on_toggle();
                                let _ = tx.send(HotkeyEvent::Pressed);
                            }
                        } else {
                            det.on_cmd_up(now);
                        }
                        *cmd_was = cmd_now;
                    } else {
                        // 其他 modifier（Shift / Option / Ctrl）變動 → 序列被打斷
                        det.on_other_event();
                    }
                }
                NSEventType::KeyDown | NSEventType::KeyUp => {
                    if let Ok(mut det) = detector.lock() {
                        det.on_other_event();
                    }
                }
                _ => {}
            }
        });

    // NSEvent::addGlobalMonitorForEventsMatchingMask_handler 在 objc2-app-kit 0.3 為
    // safe wrapper（回傳 Option<Retained<AnyObject>>）；token 必須持有到 removeMonitor。
    let token = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &handler);

    let monitor = token.ok_or_else(|| RaflowError::HotkeyRegister {
        detail: "addGlobalMonitorForEventsMatchingMask_handler returned nil (輔助使用權限可能未授予)".into(),
    })?;

    Ok(HotkeyHandle {
        monitor: Some(monitor),
    })
}

/// Edit Guard v1 使用者接管活動監看器的擁有者；drop 時 `removeMonitor` 解除。
///
/// 由 `raflow-app` 在**每次錄音開始**時建立、**停止**時 drop（設計 §5：監看隨錄音起停；
/// §7 隱私：只在錄音期間啟用）。與 `HotkeyHandle` 為各自獨立的 NSEvent 監聽，互不干擾。
pub struct ActivityMonitorHandle {
    monitor: Option<Retained<AnyObject>>,
}

impl Drop for ActivityMonitorHandle {
    fn drop(&mut self) {
        if let Some(token) = self.monitor.take() {
            // SAFETY: removeMonitor 接受先前由 addGlobalMonitorForEventsMatchingMask_handler
            // 回傳的同一個 token；我們持有唯一 strong ref，drop 時消耗註冊。
            unsafe {
                NSEvent::removeMonitor(&token);
            }
        }
    }
}

/// 註冊「使用者接管」全域監看：滑鼠按下（左/右）或導覽鍵按下 → `tx.send(())`。
///
/// 只認 raflow **從不注入**的事件（滑鼠、方向鍵/Home/End/PageUp/Down），故零誤判、
/// 免自我濾除（設計 §3）。一般可列印字元 / Backspace（raflow 會注入）一律忽略。
/// 必須於主執行緒呼叫（NSEvent global monitor 契約）。
pub fn register_activity_monitor(
    tx: UnboundedSender<()>,
) -> Result<ActivityMonitorHandle, RaflowError> {
    let _mtm = MainThreadMarker::new().ok_or_else(|| RaflowError::HotkeyRegister {
        detail: "register_activity_monitor() must be called from the main thread".into(),
    })?;

    let mask = NSEventMask::LeftMouseDown | NSEventMask::RightMouseDown | NSEventMask::KeyDown;

    let handler: RcBlock<dyn Fn(NonNull<NSEvent>)> =
        RcBlock::new(move |event_ptr: NonNull<NSEvent>| {
            // SAFETY: Apple 對 addGlobalMonitorForEvents 的 handler block 契約保證
            // event 參數在 callback 期間為有效非空 NSEvent。
            let event: &NSEvent = unsafe { event_ptr.as_ref() };
            let is_takeover = match event.r#type() {
                // raflow 從不注入滑鼠事件 → 滑鼠按下必為使用者。
                NSEventType::LeftMouseDown | NSEventType::RightMouseDown => true,
                // 讀 kCGEventSourceUserData 自我濾除：raflow 自身注入的按鍵帶
                // RAFLOW_INJECT_MARKER，其餘（真人按鍵，任何鍵）皆算接管；讀不到則退回
                // 導覽鍵安全子集（見 activity::key_is_user_takeover）。
                NSEventType::KeyDown => {
                    let user_data = event.CGEvent().map(|cg| {
                        CGEvent::integer_value_field(Some(&cg), CGEventField::EventSourceUserData)
                    });
                    key_is_user_takeover(user_data, event.keyCode())
                }
                _ => false,
            };
            if is_takeover {
                // 接收端（printer thread）drop 後 send 失敗屬正常，忽略。
                let _ = tx.send(());
            }
        });

    let token = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &handler);
    let monitor = token.ok_or_else(|| RaflowError::HotkeyRegister {
        detail: "activity monitor addGlobalMonitorForEvents returned nil (輔助使用權限可能未授予)".into(),
    })?;

    Ok(ActivityMonitorHandle {
        monitor: Some(monitor),
    })
}
