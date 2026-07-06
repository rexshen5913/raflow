//! Phase 6b：原生 macOS NSPanel 浮動視窗，顯示完整 partial / final 文字。
//!
//! 詳見 `docs/spec/overlay.md` §8 與 ADR-0005。本模組是 raflow-app 內**唯一**允許
//! `unsafe { ... }` block 的地方（憲法 §3.3 例外四）；對外公開 API 完全 safe。
//!
//! Threading：所有方法只能在主執行緒呼叫；`new()` 內以 `MainThreadMarker::new()`
//! 驗證，非主執行緒回 `Err`，避免 panic。
//!
//! Note：objc2 0.6 把多數 AppKit setter 標為 safe wrapper（內部已驗證 thread safety），
//! 故大部分 NS* 操作不需 `unsafe` block；少數仍需 unsafe 的會明確標註 SAFETY 註解。

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSPanel, NSScreen, NSTextField, NSWindowLevel,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use raflow_core::RaflowError;

const PANEL_WIDTH: f64 = 720.0;
const PANEL_HEIGHT: f64 = 140.0;
const PANEL_BOTTOM_OFFSET: f64 = 120.0;
const TEXT_INSET: f64 = 18.0;
const TEXT_FONT_SIZE: f64 = 16.0;
/// macOS `NSFloatingWindowLevel` = 3。
const FLOATING_WINDOW_LEVEL: NSWindowLevel = 3;

/// 浮動 overlay panel（HUD 風格）。一個 raflow process 只持一個實例。
///
/// 公開 API 全 safe；unsafe FFI 集中於模組內，每處附 SAFETY 註解。
pub struct FloatingOverlay {
    panel: Retained<NSPanel>,
    text_field: Retained<NSTextField>,
}

// SAFETY: NSPanel / NSTextField 並非 Send/Sync，但 raflow 把所有 overlay 操作都鎖在
// 主執行緒（new() 內以 MainThreadMarker 驗證；後續 update_text / show / hide 由 main
// thread 收到 UserEvent 後直接呼叫）。型別系統若要 enforce 這個 invariant，會需要
// `*mut` 或 PhantomData，本 MVP 信任「呼叫端只在 main thread」這個 documented contract。
unsafe impl Send for FloatingOverlay {}
unsafe impl Sync for FloatingOverlay {}

impl FloatingOverlay {
    pub fn new() -> Result<Self, RaflowError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| RaflowError::TextInject {
            detail: "FloatingOverlay::new must be called on the main thread".into(),
        })?;
        let (panel, text_field) = build_panel(mtm);
        Ok(Self { panel, text_field })
    }

    pub fn update_text(&self, text: &str) {
        let ns_text = NSString::from_str(text);
        self.text_field.setStringValue(&ns_text);
    }

    pub fn show(&self) {
        // orderFrontRegardless 不會 activate（已用 NonactivatingPanel style mask）→ 不偷 focus
        self.panel.orderFrontRegardless();
    }

    pub fn hide(&self) {
        self.panel.orderOut(None);
    }

    pub fn clear(&self) {
        self.update_text("");
    }
}

/// 內部 helper：建立 NSPanel + 嵌入 NSTextField，回 (panel, text_field) 配對。
/// 呼叫端必須在主執行緒（透過 MainThreadMarker 強制）。
fn build_panel(mtm: MainThreadMarker) -> (Retained<NSPanel>, Retained<NSTextField>) {
    // 算 frame：螢幕底部置中
    let screen_frame: NSRect = match NSScreen::mainScreen(mtm) {
        Some(screen) => screen.frame(),
        None => NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1440.0, 900.0)),
    };
    let x = screen_frame.origin.x + (screen_frame.size.width - PANEL_WIDTH) / 2.0;
    let y = screen_frame.origin.y + PANEL_BOTTOM_OFFSET;
    let panel_frame = NSRect::new(NSPoint::new(x, y), NSSize::new(PANEL_WIDTH, PANEL_HEIGHT));

    // NonactivatingPanel: 顯示時不偷 focus
    // Borderless: 無標題列、無邊框
    let style_mask = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;

    let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
        NSPanel::alloc(mtm),
        panel_frame,
        style_mask,
        NSBackingStoreType::Buffered,
        false,
    );

    // 以下 setter 由 objc2 0.6 標為 safe（內部已驗證 thread + 引數合法性）
    panel.setLevel(FLOATING_WINDOW_LEVEL);
    panel.setOpaque(false);
    panel.setHasShadow(true);
    panel.setIgnoresMouseEvents(true);
    let bg = NSColor::colorWithCalibratedWhite_alpha(0.96, 0.92);
    panel.setBackgroundColor(Some(&bg));

    // 內嵌 NSTextField
    let text_frame = NSRect::new(
        NSPoint::new(TEXT_INSET, TEXT_INSET),
        NSSize::new(
            PANEL_WIDTH - TEXT_INSET * 2.0,
            PANEL_HEIGHT - TEXT_INSET * 2.0,
        ),
    );
    let text_field: Retained<NSTextField> =
        NSTextField::initWithFrame(NSTextField::alloc(mtm), text_frame);

    text_field.setEditable(false);
    text_field.setBezeled(false);
    text_field.setBordered(false);
    text_field.setDrawsBackground(false);
    text_field.setSelectable(false);
    let fg = NSColor::colorWithCalibratedWhite_alpha(0.12, 1.0);
    text_field.setTextColor(Some(&fg));
    let font = NSFont::systemFontOfSize(TEXT_FONT_SIZE);
    text_field.setFont(Some(&font));

    if let Some(content_view) = panel.contentView() {
        content_view.addSubview(&text_field);
    }

    (panel, text_field)
}
