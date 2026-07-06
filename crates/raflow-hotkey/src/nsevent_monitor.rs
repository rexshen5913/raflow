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

pub fn register(tx: UnboundedSender<HotkeyEvent>) -> Result<HotkeyHandle, RaflowError> {
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
        detail: "addGlobalMonitorForEventsMatchingMask_handler returned nil (Input Monitoring 權限可能未授予)".into(),
    })?;

    Ok(HotkeyHandle {
        monitor: Some(monitor),
    })
}
