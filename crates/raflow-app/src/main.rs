//! raflow CLI 主程式。
//!
//! 架構見 `docs/spec/app.md §6.2`：
//!   - 主執行緒：tao EventLoop<UserEvent> + tray-icon（menu bar 常駐，Quit menu）
//!   - printer thread：消費 `TranscriptUpdate` → stdout + 注入 focus + 複製剪貼簿
//!   - worker thread：獨佔 `AppleSpeechBackend` + `App`；發 UserEvent 讓主執行緒換 tray icon

fn main() {
    #[cfg(target_os = "macos")]
    {
        if let Err(err) = mac::run() {
            mac::report_error("raflow startup failed", &err);
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("raflow is macOS-only (target: aarch64-apple-darwin)");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod accessibility;

#[cfg(target_os = "macos")]
mod floating_overlay;

#[cfg(target_os = "macos")]
mod input_source;

#[cfg(target_os = "macos")]
mod mac {
    use crate::accessibility::{FocusDetection, detect_focus, ensure_trusted_with_prompt};
    use crate::floating_overlay::FloatingOverlay;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use raflow_app::{App, Transition};
    use raflow_audio::CaptureHandle;
    use raflow_core::{AudioFrame, HotkeyEvent, RaflowError, TranscriptUpdate};
    use raflow_input::{
        ArboardClipboard, ClipboardBackend, EnigoBackend, InputBackend, PhraseEvent, PhrasePrinter,
        Replacements, StreamDiff, apply_replacements, parse_replacements,
    };
    use raflow_speech::{AppleSpeechBackend, WhisperContext, resolve_model_path, rolling_enabled};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    const LOCALE: &str = "zh-TW";
    const QUIT_MENU_ID: &str = "quit";
    /// Whisper 餵的語言：強制 `zh` 中文 tokenizer，避免使用者反映的「偶爾出現韓文」
    /// （`auto` 模式下 Whisper 會自己 detect，相近 prosody 可能誤判 ko/ja）。
    /// 中英混合靠 `set_initial_prompt` 引導 + 結果 safety filter 雙保險。
    /// 詳見 docs/spec/whisper.md §11 §12。
    const WHISPER_LANGUAGE: &str = "zh";

    const ICON_IDLE: &[u8] = include_bytes!("../../../packaging/icons/menubar-idle@2x.png");
    const ICON_RECORDING: &[u8] =
        include_bytes!("../../../packaging/icons/menubar-recording@2x.png");

    /// tao 自訂事件：worker / menu / printer 透過 `EventLoopProxy` 發送給主執行緒。
    #[derive(Debug, Clone)]
    enum UserEvent {
        RecordingStarted,
        RecordingStopped,
        MenuClick(String),
        /// Phase 6a/6b：menu bar 圖示旁 + 浮動視窗同時顯示 partial 文字
        /// （即時可見的視覺反饋；不在輸入框時也能看到）。`None` 清空 + 立即 hide。
        /// 詳見 docs/spec/overlay.md。
        OverlayText(Option<String>),
        /// Phase 6b：排程 floating overlay 在指定延遲後 hide（讓使用者讀完 final）。
        /// 期間若有新的 OverlayText(Some) 抵達，自動取消 pending hide。
        OverlayScheduleHide(Duration),
    }

    /// 把 `RaflowError` 映射到對應的 System Settings 深連結引導文字。
    pub fn permission_hint(err: &RaflowError) -> Option<&'static str> {
        match err {
            RaflowError::SpeechAuthorization { .. } => Some(
                "→ 開啟系統設定至「隱私權與安全性 → 語音辨識」授權 raflow：\n  \
                 open 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_SpeechRecognition'",
            ),
            RaflowError::AudioCapture { .. } => Some(
                "→ 若是麥克風權限問題，開啟「隱私權與安全性 → 麥克風」授權 raflow：\n  \
                 open 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Microphone'",
            ),
            RaflowError::TextInject { .. } => Some(
                "→ enigo 透過 CGEvent 模擬鍵盤，需「輔助使用（Accessibility）」授權：\n  \
                 open 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Accessibility'",
            ),
            RaflowError::HotkeyRegister { .. } => Some(
                "→ 雙擊 Cmd 偵測註冊失敗。多半是 Input Monitoring 權限未授予：\n  \
                 open 'x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_ListenEvent'",
            ),
            RaflowError::ClipboardWrite { .. }
            | RaflowError::SpeechUnavailable { .. }
            | RaflowError::SpeechBusy
            | RaflowError::ConfigLoad { .. }
            | RaflowError::WhisperModelMissing { .. }
            | RaflowError::WhisperLoad { .. }
            | RaflowError::WhisperInference { .. } => None,
        }
    }

    /// 標準的錯誤輸出格式：`{prefix}: {err}`，並於有 hint 時附加引導。
    pub fn report_error(prefix: &str, err: &RaflowError) {
        eprintln!("{prefix}: {err}");
        if let Some(hint) = permission_hint(err) {
            eprintln!("{hint}");
        }
    }

    fn build_current_thread_rt() -> Result<tokio::runtime::Runtime, RaflowError> {
        tokio::runtime::Builder::new_current_thread()
            // enable_time：worker 的 rolling 計時器（tokio::time::interval）需要 time driver。
            // 對 printer / auth 的 block_on 無害。
            .enable_time()
            .build()
            .map_err(|e| RaflowError::AudioCapture {
                detail: format!("failed to build tokio runtime: {e}"),
            })
    }

    fn spawn_error(detail: impl std::fmt::Display) -> RaflowError {
        RaflowError::AudioCapture {
            detail: detail.to_string(),
        }
    }

    fn decode_icon(bytes: &[u8], label: &str) -> Result<Icon, RaflowError> {
        let img = image::load_from_memory(bytes).map_err(|e| RaflowError::AudioCapture {
            detail: format!("failed to decode {label} icon: {e}"),
        })?;
        let rgba = img.to_rgba8();
        let (width, height) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), width, height).map_err(|e| RaflowError::AudioCapture {
            detail: format!("failed to build {label} icon: {e}"),
        })
    }

    /// Phase 6b：Final / Error 後浮動視窗保留多久才 hide（讓使用者讀完）。
    const OVERLAY_HIDE_DELAY: Duration = Duration::from_secs(3);

    /// Phase 2 句級滾動 tick 週期（ADR-0006 §8.7.2）。停頓即鎖只在 tick 邊界偵測，故週期
    /// 越短鎖定越即時；1s 兼顧反應速度與每 tick 的 VAD 重跑成本。rolling OFF 時 tick 仍
    /// 觸發但 backend 立即 no-op（成本可忽略）。
    const ROLLING_TICK_INTERVAL: Duration = Duration::from_millis(1000);

    /// 使用者「取代對照表」路徑：env `RAFLOW_REPLACEMENTS` 優先，否則預設
    /// `$HOME/Library/Application Support/raflow/replacements.txt`。
    fn replacements_path() -> Option<std::path::PathBuf> {
        if let Some(p) = std::env::var_os("RAFLOW_REPLACEMENTS") {
            return Some(std::path::PathBuf::from(p));
        }
        let home = std::env::var_os("HOME")?;
        let mut p = std::path::PathBuf::from(home);
        p.push("Library");
        p.push("Application Support");
        p.push("raflow");
        p.push("replacements.txt");
        Some(p)
    }

    /// 載入取代對照表（檔不存在/讀失敗 → 空表）。每次 SessionStarted 重讀 → 改檔即生效。
    fn load_replacements() -> Replacements {
        let contents = replacements_path().and_then(|p| std::fs::read_to_string(p).ok());
        contents
            .as_deref()
            .map(parse_replacements)
            .unwrap_or_default()
    }

    /// 執行 printer reducer 算出的注入動作（先 backspace 再 append）。
    /// `input` 為 `None`（EnigoBackend init 失敗）時純 no-op；錯誤只記錄不中斷 session。
    fn exec_inject(input: &mut Option<EnigoBackend>, diff: &StreamDiff) {
        let Some(backend) = input.as_mut() else {
            return;
        };
        if diff.backspace > 0 {
            if let Err(err) = backend.backspace(diff.backspace) {
                report_error("! stream inject failed (backspace)", &err);
                return;
            }
        }
        if !diff.append.is_empty() {
            if let Err(err) = backend.inject(&diff.append) {
                report_error("! stream inject failed (append)", &err);
            }
        }
    }

    fn run_printer(
        mut transcript_rx: UnboundedReceiver<TranscriptUpdate>,
        proxy: EventLoopProxy<UserEvent>,
    ) {
        let rt = match build_current_thread_rt() {
            Ok(rt) => rt,
            Err(err) => {
                report_error("printer thread failed to start", &err);
                return;
            }
        };
        let mut input: Option<EnigoBackend> = match EnigoBackend::new() {
            Ok(backend) => Some(backend),
            Err(err) => {
                report_error("! text injection disabled (init failed)", &err);
                None
            }
        };
        let mut clipboard: Option<ArboardClipboard> = match ArboardClipboard::new() {
            Ok(cb) => Some(cb),
            Err(err) => {
                report_error("! clipboard fallback disabled (init failed)", &err);
                None
            }
        };
        // 句級滾動 printer（ADR-0006 §2.5）。committed = 已鎖定前綴（PhraseFinal / Final
        // 累積），last_partial = 當前未定稿句草稿。backspace 上限恆為 last_partial 長度，
        // 游標永不退回 committed（不刪已鎖定/手改內容）。詳見 docs/spec/input.md §3。
        //
        // 相容性：目前 backend 只送 SessionStarted/Partial/Final/Error（未送 PhraseFinal）。
        // 該情境下 committed 恆為空，故各分支的 inject / clipboard / overlay 輸出與 HEAD
        // 的整段校正版逐位元相同；PhraseFinal 分支為句級滾動（Phase 2 pipeline）預留。
        let mut printer = PhrasePrinter::new();
        // 使用者取代對照表（術語硬性修正，如「阿狗CD」→「ArgoCD」）。每次 SessionStarted 重讀
        // → 改檔後下次錄音即生效。套用在餵給 printer 之前，故 partial / final / overlay / 剪貼簿
        // 都是修正後文字。詳見 docs/spec/input.md。
        let mut replacements = load_replacements();
        rt.block_on(async move {
            while let Some(update) = transcript_rx.recv().await {
                match update {
                    TranscriptUpdate::SessionStarted => {
                        // 新一輪錄音開始：清空鎖定前綴與草稿，避免上次殘留算出錯誤 backspace
                        // （spec/input.md §3）。reducer 回 no-op，不對已輸入內容 backspace。
                        replacements = load_replacements(); // 重讀 → 改檔即生效
                        let _ = printer.apply(PhraseEvent::SessionStarted);
                        let _ = proxy.send_event(UserEvent::OverlayText(None));
                    }
                    TranscriptUpdate::Partial(text) => {
                        let text = apply_replacements(&text, &replacements);
                        println!("~ {text}");
                        let diff = printer.apply(PhraseEvent::Partial(&text));
                        exec_inject(&mut input, &diff);
                        // Floating panel 顯示「已鎖定前綴 + 當前草稿」；面板自己 wrap，不截斷。
                        let shown = format!("{}{}", printer.committed(), printer.last_partial());
                        let _ = proxy.send_event(UserEvent::OverlayText(Some(shown)));
                    }
                    TranscriptUpdate::PhraseFinal(text) => {
                        // 句級定稿：對齊當前草稿 → 鎖定進 committed，草稿清空。session 續錄，
                        // 不寫剪貼簿、不排程 hide（那是 Final 的事）。
                        let text = apply_replacements(&text, &replacements);
                        println!("= {text}");
                        let diff = printer.apply(PhraseEvent::PhraseFinal(&text));
                        exec_inject(&mut input, &diff);
                        let shown = printer.committed().to_string();
                        let _ = proxy.send_event(UserEvent::OverlayText(Some(shown)));
                    }
                    TranscriptUpdate::Final(text) => {
                        let text = apply_replacements(&text, &replacements);
                        println!("= {text}");
                        let diff = printer.apply(PhraseEvent::Final(&text));
                        exec_inject(&mut input, &diff);
                        // 整段 = committed（含所有已鎖定句 + 本次收尾句）。
                        let whole = printer.committed().to_string();
                        if let Some(cb) = clipboard.as_mut() {
                            if let Err(err) = cb.copy(&whole) {
                                report_error("! clipboard write failed", &err);
                            }
                        }
                        // Final 顯示完整文字，3 秒後 hide（讓使用者讀完）
                        let _ = proxy.send_event(UserEvent::OverlayText(Some(whole)));
                        let _ =
                            proxy.send_event(UserEvent::OverlayScheduleHide(OVERLAY_HIDE_DELAY));
                    }
                    TranscriptUpdate::Error(msg) => {
                        eprintln!("! speech error: {msg}");
                        let _ = printer.apply(PhraseEvent::Error);
                        let _ = proxy.send_event(UserEvent::OverlayText(Some(format!("! {msg}"))));
                        let _ =
                            proxy.send_event(UserEvent::OverlayScheduleHide(OVERLAY_HIDE_DELAY));
                    }
                }
            }
        });
    }

    /// 嘗試載入 Whisper 終校 context；spec/whisper.md §4 §7。
    /// - 沒 `whisper` feature → stub 永遠回 Err，本函式回 None（呼叫端跳過）
    /// - feature on 但 model 不存在 / load 失敗 → log warn 並回 None，啟動繼續
    /// - 成功 → log info 並回 `Some(Arc<...>)`，並 spawn background thread 跑
    ///   1 秒 silence 觸發 CoreML 編譯到 ANE 並 cache（之後第一次真正 transcribe 才不會卡）
    fn try_load_whisper() -> Option<Arc<WhisperContext>> {
        let path = resolve_model_path()?;
        if !path.exists() {
            eprintln!(
                "raflow: whisper disabled — model not found at {}\n  \
                 → 跑 `make whisper-model-small` 下載 ggml-small.bin (~466 MB)\n  \
                 → 或設 RAFLOW_WHISPER_MODEL=/path/to/your.bin",
                path.display()
            );
            return None;
        }
        match WhisperContext::load(&path, WHISPER_LANGUAGE) {
            Ok(ctx) => {
                eprintln!("raflow: whisper enabled (model: {})", path.display());
                let ctx = Arc::new(ctx);
                spawn_whisper_warmup(ctx.clone());
                Some(ctx)
            }
            Err(err) => {
                eprintln!(
                    "raflow: whisper disabled — load failed: {err}\n  \
                     (回退到 Apple Speech only；確認 raflow-app 編譯時是否帶 --features whisper)"
                );
                None
            }
        }
    }

    /// CoreML warm-up：第一次 inference 會把 model 編譯成設備特定的 ANE 格式並 cache 到
    /// `~/Library/Caches/com.apple.coreml/`，之後 load 直接用 cached binary（< 1s）。
    /// 啟動時 background thread 跑一次，讓使用者第一次真正錄音的 final 不會卡幾分鐘。
    /// 詳見 docs/spec/whisper.md §10 補充 / docs/todo.md。
    fn spawn_whisper_warmup(ctx: Arc<WhisperContext>) {
        thread::Builder::new()
            .name("raflow-whisper-warmup".into())
            .spawn(move || {
                eprintln!(
                    "raflow: whisper warming up CoreML (一次性，首次可能 30s~幾分鐘；後續啟動 < 1s)..."
                );
                let started = std::time::Instant::now();
                // 1 秒 16 kHz mono silence — 走完整 inference 路徑，但語意上不產出文字
                let silence = vec![0_i16; 16_000];
                match ctx.transcribe(&silence) {
                    Ok(_) => eprintln!(
                        "raflow: whisper warm-up complete in {:.1}s",
                        started.elapsed().as_secs_f32()
                    ),
                    Err(err) => eprintln!(
                        "raflow: whisper warm-up failed: {err} (功能仍可用，只是第一次 final 會慢)"
                    ),
                }
            })
            .ok();
    }

    async fn worker_loop(
        mut hotkey_rx: UnboundedReceiver<HotkeyEvent>,
        mut audio_rx: UnboundedReceiver<AudioFrame>,
        audio_tx: UnboundedSender<AudioFrame>,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<(), RaflowError> {
        let backend = AppleSpeechBackend::new(LOCALE)?;
        let backend = match try_load_whisper() {
            Some(ctx) => backend.with_whisper(ctx),
            None => backend,
        };
        // Phase 2 句級滾動（RAFLOW_ROLLING）：預設 ON（經實機驗證後轉正）；
        // RAFLOW_ROLLING=0 退回「停止時整段校正」。ON 需 whisper + VAD model 才生效；
        // 缺則 rolling_tick no-op、退化為整段校正行為。
        let rolling = rolling_enabled();
        let backend = backend.with_rolling(rolling);
        if rolling {
            eprintln!(
                "raflow: 句級滾動校正 ON（預設；需 whisper + VAD model）；\n  \
                 每 {}ms 對已閉合語音段跑 Whisper 送 PhraseFinal。關閉：RAFLOW_ROLLING=0。",
                ROLLING_TICK_INTERVAL.as_millis()
            );
        }
        // 每次錄音開始時讀當前輸入法 → 自動選 zh-TW / en-US（ADR-0007 / spec/speech.md §2）。
        // backend 初始 locale 為 LOCALE（zh-TW）；首次 start 依輸入法切換。
        let mut app: App<AppleSpeechBackend> = App::with_locale_provider(
            backend,
            Box::new(crate::input_source::current_input_locale),
            LOCALE.to_string(), // fallback：preferred 語言 recognizer 不可用時退回 zh-TW
            transcript_tx,
        );
        let mut capture: Option<CaptureHandle> = None;

        // Phase 2 句級滾動計時器：錄音中週期性觸發 rolling_tick(false)。第一 tick 立即到期，
        // 用 `MissedTickBehavior::Skip` 避免久 block 後補打一串 tick（rolling OFF 時亦無害）。
        let mut rolling_timer = tokio::time::interval(ROLLING_TICK_INTERVAL);
        rolling_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = rolling_timer.tick() => {
                    // 只在錄音中轉發；非錄音 / rolling OFF → session/backend 端 no-op。
                    if app.is_recording() {
                        if let Err(err) = app.on_rolling_tick(false) {
                            report_error("! rolling tick failed", &err);
                        }
                    }
                }
                Some(event) = hotkey_rx.recv() => {
                    match app.on_hotkey(event) {
                        Ok(Transition::StartRecording) => {
                            match raflow_audio::start(audio_tx.clone()) {
                                Ok(handle) => {
                                    capture = Some(handle);
                                    eprintln!("● recording");
                                    let _ = proxy.send_event(UserEvent::RecordingStarted);
                                }
                                Err(err) => report_error("! audio start failed", &err),
                            }
                        }
                        Ok(Transition::StopRecording) => {
                            capture = None;
                            eprintln!("○ stopped");
                            let _ = proxy.send_event(UserEvent::RecordingStopped);
                        }
                        Ok(Transition::None) => {}
                        Err(err) => report_error("! hotkey handling failed", &err),
                    }
                }
                Some(frame) = audio_rx.recv() => {
                    if let Err(err) = app.on_audio_frame(&frame) {
                        report_error("! frame handling failed", &err);
                    }
                }
                else => break,
            }
        }
        drop(capture);
        Ok(())
    }

    fn run_worker(
        hotkey_rx: UnboundedReceiver<HotkeyEvent>,
        audio_rx: UnboundedReceiver<AudioFrame>,
        audio_tx: UnboundedSender<AudioFrame>,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<(), RaflowError> {
        let rt = build_current_thread_rt()?;
        rt.block_on(worker_loop(
            hotkey_rx,
            audio_rx,
            audio_tx,
            transcript_tx,
            proxy,
        ))
    }

    fn build_tray(idle_icon: Icon) -> Result<TrayIcon, RaflowError> {
        let menu = Menu::new();
        let quit_item = MenuItem::with_id(QUIT_MENU_ID, "Quit raflow", true, None);
        menu.append(&quit_item)
            .map_err(|e| RaflowError::AudioCapture {
                detail: format!("failed to build tray menu: {e}"),
            })?;
        // Idle icon 走 template（黑白剪影，由 macOS 依 menu bar 明暗 tint）；
        // Recording icon 走全彩（要顯出真正的紅色錄音點），在 set_icon 時
        // 另行 set_icon_as_template(false) 關閉 tint。
        TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("raflow")
            .with_icon(idle_icon)
            .with_icon_as_template(true)
            .build()
            .map_err(|e| RaflowError::AudioCapture {
                detail: format!("failed to build tray icon: {e}"),
            })
    }

    /// 設定 NSApplication 為 Accessory 模式：不占 Dock，但可在 menu bar 放 tray icon。
    /// 見 ADR-0003：tray-icon 官方文件明定需要此 policy。
    fn set_accessory_activation_policy() {
        // SAFETY: (1) `MainThreadMarker::new_unchecked()` 於 `mac::run()` 起始時呼叫，
        //          此 function 僅於主執行緒被呼叫（main() → mac::run()）。
        //        (2) `NSApplication::sharedApplication` 在 Apple 文件中為任意時點安全；
        //          tao 0.35 的 `EventLoopBuilder::build()` 也呼叫同一 singleton。
        //        (3) `Accessory` 為 `NSApplicationActivationPolicy` enum 合法 variant。
        unsafe {
            let mtm = MainThreadMarker::new_unchecked();
            let app = NSApplication::sharedApplication(mtm);
            app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        }
    }

    pub fn run() -> Result<(), RaflowError> {
        eprintln!("raflow: requesting speech authorization...");
        {
            let rt = build_current_thread_rt()?;
            rt.block_on(raflow_speech::request_authorization())?;
        }
        eprintln!("raflow: speech authorized.");

        let idle_icon = decode_icon(ICON_IDLE, "idle")?;
        let recording_icon = decode_icon(ICON_RECORDING, "recording")?;

        // 先設 activation policy 再 build EventLoop：sharedApplication 會 init NSApplication
        // singleton（若尚未存在），policy 在 tao 初始化 event loop 前就落地。
        set_accessory_activation_policy();
        let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();

        // Menu events flow from muda's internal channel → tao user events via proxy.
        let proxy_for_menu = proxy.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let _ = proxy_for_menu.send_event(UserEvent::MenuClick(event.id.0.clone()));
        }));

        let (hotkey_tx, hotkey_rx) = unbounded_channel::<HotkeyEvent>();
        let (audio_tx, audio_rx) = unbounded_channel::<AudioFrame>();
        let (transcript_tx, transcript_rx) = unbounded_channel::<TranscriptUpdate>();

        let proxy_for_printer = proxy.clone();
        thread::Builder::new()
            .name("raflow-printer".into())
            .spawn(move || run_printer(transcript_rx, proxy_for_printer))
            .map_err(|e| spawn_error(format!("spawn printer thread: {e}")))?;

        let proxy_for_worker = proxy.clone();
        thread::Builder::new()
            .name("raflow-worker".into())
            .spawn(move || {
                if let Err(err) = run_worker(
                    hotkey_rx,
                    audio_rx,
                    audio_tx,
                    transcript_tx,
                    proxy_for_worker,
                ) {
                    report_error("worker exited with error", &err);
                }
            })
            .map_err(|e| spawn_error(format!("spawn worker thread: {e}")))?;

        let _hotkey_handle = raflow_hotkey::register(hotkey_tx)?;
        // 主動觸發系統 Accessibility prompt（若未授權）。enigo 的 CGEventPost 在沒
        // Accessibility 時「靜默 no-op」不回錯誤，使用者會完全看不出問題。
        // `AXIsProcessTrustedWithOptions(prompt: true)` 一個 process 生命週期只跳一次。
        if !ensure_trusted_with_prompt() {
            eprintln!(
                "raflow: ⚠ Accessibility 權限未授予 → enigo 的文字注入會靜默失敗，輸入框\n  \
                 不會出現文字。剛剛應該已彈出系統 dialog，請點「開啟系統設定」並把\n  \
                 raflow 加入「隱私權與安全性 → 輔助使用」並打勾。打勾後重啟 raflow 生效。\n  \
                 （focus 偵測也會一併失敗 → floating panel 不顯示）",
            );
        }
        eprintln!("raflow: ready. double-tap Cmd to toggle recording. Quit from menu bar icon.");
        // 給使用者明顯的故障排除線索：menu bar 圖示變紅 = hotkey OK；如果輸入框沒出現
        // 文字，多半是 enigo 沒拿到 Accessibility 權限（macOS 上 enigo.text() 在沒
        // 授權時會「靜默 no-op」不回錯誤，所以 stderr 看不到任何 `!` 警告）。
        eprintln!(
            "raflow: 若雙擊 Cmd 後 menu bar 變紅但輸入框沒出現文字 → 通常是 Accessibility 權限\n  \
             → 系統設定 → 隱私權與安全性 → 輔助使用 → 加入 raflow.app 並打勾\n  \
             → 或執行：tccutil reset Accessibility dev.raflow.raflow 後重新授權",
        );

        // tray-icon 官方 doc 要求：「the earliest safe point is the StartCause::Init event」。
        // 在這之前 build 會讓 NSStatusItem 無法註冊到 NSStatusBar，icon 根本不會出現。
        let mut tray: Option<TrayIcon> = None;
        // FloatingOverlay 必須在主執行緒建（new() 內以 MainThreadMarker 驗證）；
        // 跟 tray 一起在 StartCause::Init 建。
        let mut overlay: Option<FloatingOverlay> = None;
        // Pending hide 排程：到期後關閉浮動視窗。新的 OverlayText(Some) 來會 cancel。
        let mut overlay_hide_at: Option<Instant> = None;
        // 本次錄音 session 的 focus 狀態（在 RecordingStarted 時 query 一次定案，避免每個
        // partial 都打 AX API；spec/overlay.md §8.3-fix）。預設 false 代表「不確定」→
        // 顯示 floating panel 作為視覺安全網。
        let mut session_focused_in_text_input: bool = false;

        event_loop.run(move |event, _target, control_flow| {
            // 預設 wait；若有 pending hide 則 wait 到指定時間醒來收尾
            *control_flow = match overlay_hide_at {
                Some(at) => ControlFlow::WaitUntil(at),
                None => ControlFlow::Wait,
            };

            match event {
                Event::NewEvents(StartCause::Init) => {
                    match build_tray(idle_icon.clone()) {
                        Ok(t) => tray = Some(t),
                        Err(err) => report_error("failed to build menu bar tray icon", &err),
                    }
                    match FloatingOverlay::new() {
                        Ok(o) => overlay = Some(o),
                        Err(err) => report_error("! floating overlay disabled", &err),
                    }
                }
                Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                    // 排程到期：hide overlay
                    if let Some(at) = overlay_hide_at {
                        if Instant::now() >= at {
                            if let Some(o) = overlay.as_ref() {
                                o.hide();
                            }
                            overlay_hide_at = None;
                        }
                    }
                }
                Event::UserEvent(ue) => match ue {
                    UserEvent::RecordingStarted => {
                        if let Some(tray) = tray.as_ref() {
                            tray.set_icon_as_template(false);
                            let _ = tray.set_icon(Some(recording_icon.clone()));
                        }
                        // 在 session 起點 query 一次 focus，整個 session 內所有 partial /
                        // final 都用此 cached 結果決定是否彈 floating panel。避免每個
                        // partial 都打 AX API（每次 ~5-20ms）。三狀態語意見
                        // accessibility.rs module 文件。
                        let detection = detect_focus();
                        session_focused_in_text_input = detection.suppresses_panel();
                        match &detection {
                            FocusDetection::Untrusted => eprintln!(
                                "raflow: session focus = untrusted (no Accessibility permission; panel suppressed to avoid stacking with inject)\n  \
                                 → 開啟系統設定 → 隱私權與安全性 → 輔助使用 → 加入 raflow.app 並打勾",
                            ),
                            FocusDetection::Unknown => eprintln!(
                                "raflow: session focus = unknown (AX granted but no focused element; likely Electron / hidden AX tree → panel suppressed, clipboard fallback still works via Cmd+V)",
                            ),
                            FocusDetection::Detected(info) => eprintln!(
                                "raflow: session focus = {} (AXRole={:?})",
                                if info.editable {
                                    "text input (panel suppressed)"
                                } else {
                                    "non-text (panel will show)"
                                },
                                info.role,
                            ),
                        }
                    }
                    UserEvent::RecordingStopped => {
                        if let Some(tray) = tray.as_ref() {
                            tray.set_icon_as_template(true);
                            let _ = tray.set_icon(Some(idle_icon.clone()));
                        }
                        // 不立即 hide overlay：let Final 抵達後的 ScheduleHide 處理。
                        // 若使用者極短錄音沒收到 Final，下一次 SessionStarted 會 clear+重 show。
                    }
                    UserEvent::OverlayText(text) => {
                        // Menu bar 不顯示文字（使用者反饋：太擾人）；只用紅色圖示切換做狀態
                        // 指示。partial / final 視覺反饋集中在 floating panel（focus 不在
                        // 輸入框時）或輸入框本身（focus 在時）。
                        // Floating panel 只在「focus 不在輸入框」時顯示，作為視覺安全網
                        if let Some(o) = overlay.as_ref() {
                            match text.as_deref() {
                                Some(t) if !session_focused_in_text_input => {
                                    o.update_text(t);
                                    o.show();
                                    overlay_hide_at = None; // 取消任何 pending hide
                                }
                                Some(_) => {
                                    // focus 在輸入框 → 抑制 panel；確保 cleared & hidden
                                    o.hide();
                                    overlay_hide_at = None;
                                }
                                None => {
                                    o.hide();
                                    o.clear();
                                    overlay_hide_at = None;
                                }
                            }
                        }
                    }
                    UserEvent::OverlayScheduleHide(d) => {
                        overlay_hide_at = Some(Instant::now() + d);
                    }
                    UserEvent::MenuClick(id) => {
                        if id == QUIT_MENU_ID {
                            *control_flow = ControlFlow::Exit;
                        }
                    }
                },
                _ => {}
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn permission_hint_covers_all_actionable_errors() {
            let cases: Vec<(RaflowError, &str)> = vec![
                (
                    RaflowError::SpeechAuthorization {
                        status: "denied".into(),
                    },
                    "語音辨識",
                ),
                (
                    RaflowError::AudioCapture {
                        detail: "no input device".into(),
                    },
                    "麥克風",
                ),
                (
                    RaflowError::TextInject {
                        detail: "accessibility denied".into(),
                    },
                    "輔助使用",
                ),
                (
                    RaflowError::HotkeyRegister {
                        detail: "in use".into(),
                    },
                    "Input Monitoring",
                ),
            ];
            for (err, keyword) in cases {
                let hint = permission_hint(&err).unwrap_or_else(|| {
                    panic!("expected hint for {err:?}");
                });
                assert!(
                    hint.contains(keyword),
                    "hint for {err:?} should mention {keyword:?}, got: {hint}",
                );
                assert!(
                    hint.contains("x-apple.systempreferences:"),
                    "hint for {err:?} should include a System Settings deep link",
                );
            }
        }

        #[test]
        fn permission_hint_is_none_for_non_actionable_errors() {
            let cases = [
                RaflowError::SpeechBusy,
                RaflowError::SpeechUnavailable {
                    locale: "zh-TW".into(),
                },
                RaflowError::ClipboardWrite {
                    detail: "NSPasteboard error".into(),
                },
            ];
            for err in cases {
                assert!(
                    permission_hint(&err).is_none(),
                    "{err:?} should have no actionable hint",
                );
            }
        }
    }
}
