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
mod correction_popover;

#[cfg(target_os = "macos")]
mod floating_overlay;

#[cfg(target_os = "macos")]
mod input_source;

#[cfg(target_os = "macos")]
mod settings;

#[cfg(target_os = "macos")]
mod mac {
    use crate::accessibility::{
        FocusDetection, detect_focus, ensure_trusted_with_prompt, frontmost_app_pid,
    };
    use crate::floating_overlay::FloatingOverlay;
    use crate::settings::{self, Settings};
    use arc_swap::ArcSwap;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use raflow_app::{App, Transition};
    use raflow_audio::CaptureHandle;
    use raflow_core::{AudioFrame, HotkeyEvent, RaflowError, TranscriptUpdate};
    use raflow_input::{
        ArboardClipboard, ClipboardBackend, EnigoBackend, FocusGuard, InputBackend, PhraseEvent,
        PhrasePrinter, RecentTokens, Replacements, StreamDiff, apply_replacements,
        parse_replacements, upsert_contextual_priority_term_file, upsert_replacement_file,
    };
    use raflow_speech::{AppleSpeechBackend, WhisperContext, resolve_model_path};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use tao::event::{Event, StartCause};
    use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
    use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

    const LOCALE: &str = "zh-TW";
    const QUIT_MENU_ID: &str = "quit";
    /// Menu 設定開關與「編輯…」項的 id（spec/settings.md §5）。
    const MENU_ID_AUTO_LOCALE: &str = "settings.auto_locale";
    const MENU_ID_WHISPER_CORRECTION: &str = "settings.whisper_correction";
    const MENU_ID_EDIT_TERMS: &str = "edit.terms";
    const MENU_ID_EDIT_REPLACEMENTS: &str = "edit.replacements";
    /// D1「教一個更正」擷取 popover 觸發（docs/design/vocabulary-growth.md §3）。
    const MENU_ID_TEACH_CORRECTION: &str = "edit.teach_correction";
    /// 「最近注入英文 token」候選緩衝保留的句數（供更正 popover 的「聽成」下拉）。
    const RECENT_TOKENS_CAP: usize = 5;
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
        /// D1：printer 發布「本 session 最近注入英文 token」候選快照給主執行緒（供更正 popover
        /// 的「聽成」下拉）。經 channel 傳遞（憲法 §4.1），主執行緒只持唯讀副本、不跨緒共享記憶體。
        RecentTokensUpdated(Vec<String>),
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
            | RaflowError::WhisperInference { .. }
            | RaflowError::InvalidReplacement { .. }
            | RaflowError::ConfigWrite { .. } => None,
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
    /// 經 [`FocusGuard`] 檢查後才注入（security audit run-1 Finding 1 修復）：前景 app
    /// 與 session 起點不同 → 跳過注入並閂住整個 session（防止 backspace 對不上毀損文字），
    /// 閂鎖觸發那一次印 stderr 提示。剪貼簿與 overlay 不經此函式，不受影響。
    fn exec_inject_guarded(
        guard: &mut FocusGuard,
        input: &mut Option<EnigoBackend>,
        diff: &StreamDiff,
    ) {
        let was_latched = guard.latched();
        if guard.should_inject(frontmost_app_pid()) {
            exec_inject(input, diff);
        } else if !was_latched {
            eprintln!(
                "! focus 已切到其他 app —— 本輪錄音的文字注入停止（避免打進錯的視窗）；\n  \
                 停止錄音後全文仍會複製到剪貼簿，可 Cmd+V 取回"
            );
        }
    }

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
        // 注入焦點守衛（Finding 1）：SessionStarted 記下前景 app PID 基準，之後每次注入
        // 前比對；使用者中途 Cmd+Tab 切走 → 本 session 注入停止（閂鎖），不打進錯的 app。
        let mut focus_guard = FocusGuard::new();
        // D1：本 session「最近注入英文 token」候選緩衝（記憶體、有上限、session 結束即棄）。
        // 每句定稿後 push，並把候選快照經 proxy 發布給主執行緒（更正 popover 的「聽成」下拉）。
        let mut recent_tokens = RecentTokens::new(RECENT_TOKENS_CAP);
        rt.block_on(async move {
            while let Some(update) = transcript_rx.recv().await {
                match update {
                    TranscriptUpdate::SessionStarted => {
                        // 新一輪錄音開始：清空鎖定前綴與草稿，避免上次殘留算出錯誤 backspace
                        // （spec/input.md §3）。reducer 回 no-op，不對已輸入內容 backspace。
                        replacements = load_replacements(); // 重讀 → 改檔即生效
                        focus_guard.session_started(frontmost_app_pid());
                        let _ = printer.apply(PhraseEvent::SessionStarted);
                        let _ = proxy.send_event(UserEvent::OverlayText(None));
                        // 新錄音 session：候選緩衝清空（§9 隱私：不跨 session 殘留），並通知主執行緒清空。
                        recent_tokens = RecentTokens::new(RECENT_TOKENS_CAP);
                        let _ = proxy.send_event(UserEvent::RecentTokensUpdated(Vec::new()));
                    }
                    TranscriptUpdate::Partial(text) => {
                        let text = apply_replacements(&text, &replacements);
                        println!("~ {text}");
                        let diff = printer.apply(PhraseEvent::Partial(&text));
                        exec_inject_guarded(&mut focus_guard, &mut input, &diff);
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
                        exec_inject_guarded(&mut focus_guard, &mut input, &diff);
                        let shown = printer.committed().to_string();
                        let _ = proxy.send_event(UserEvent::OverlayText(Some(shown)));
                        // D1：句級定稿即注入完成 → 收進候選緩衝並發布快照。
                        recent_tokens.push_sentence(&text);
                        let _ = proxy
                            .send_event(UserEvent::RecentTokensUpdated(recent_tokens.candidates()));
                    }
                    TranscriptUpdate::Final(text) => {
                        let text = apply_replacements(&text, &replacements);
                        println!("= {text}");
                        let diff = printer.apply(PhraseEvent::Final(&text));
                        exec_inject_guarded(&mut focus_guard, &mut input, &diff);
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
                        // D1：收尾句也收進候選緩衝並發布快照。
                        recent_tokens.push_sentence(&text);
                        let _ = proxy
                            .send_event(UserEvent::RecentTokensUpdated(recent_tokens.candidates()));
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
        user_settings: Arc<ArcSwap<Settings>>,
    ) -> Result<(), RaflowError> {
        let backend = AppleSpeechBackend::new(LOCALE)?;
        let backend = match try_load_whisper() {
            Some(ctx) => backend.with_whisper(ctx),
            None => backend,
        };
        // Phase 2 句級滾動：由 menu「Whisper 智慧校正」開關控制（spec/settings.md §4，
        // RAFLOW_ROLLING env 已於啟動時摺疊進 settings 初始值）。閘門每次錄音 start
        // 讀值；OFF → 滾動與整段終校皆跳過（所見即所得）。需 whisper + VAD model
        // 才生效；缺則 rolling_tick no-op、退化為整段校正行為。
        let gate_settings = user_settings.clone();
        let backend = backend
            .with_rolling(true)
            .with_correction_gate(Arc::new(move || gate_settings.load().whisper_correction));
        if user_settings.load().whisper_correction {
            eprintln!(
                "raflow: Whisper 智慧校正 ON（句級滾動；需 whisper + VAD model）；\n  \
                 每 {}ms 對已閉合語音段跑 Whisper 送 PhraseFinal。可由 menu bar 切換。",
                ROLLING_TICK_INTERVAL.as_millis()
            );
        } else {
            eprintln!("raflow: Whisper 智慧校正 OFF（所見即所得）；可由 menu bar 開啟。");
        }
        // 每次錄音開始時決定 locale（ADR-0007 / spec/speech.md §2 / settings.md §4）：
        // 「依輸入法自動切換」ON → 讀當前輸入法；OFF → 固定 zh-TW。
        let locale_settings = user_settings.clone();
        let mut app: App<AppleSpeechBackend> = App::with_locale_provider(
            backend,
            Box::new(move || {
                if locale_settings.load().auto_locale {
                    crate::input_source::current_input_locale()
                } else {
                    LOCALE.to_string()
                }
            }),
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
        user_settings: Arc<ArcSwap<Settings>>,
    ) -> Result<(), RaflowError> {
        let rt = build_current_thread_rt()?;
        rt.block_on(worker_loop(
            hotkey_rx,
            audio_rx,
            audio_tx,
            transcript_tx,
            proxy,
            user_settings,
        ))
    }

    /// Menu 設定開關的 handle：點擊事件時讀 `is_checked()` 回寫 ArcSwap + 檔案。
    struct MenuHandles {
        auto_locale: CheckMenuItem,
        whisper_correction: CheckMenuItem,
    }

    fn build_tray(
        idle_icon: Icon,
        initial: Settings,
    ) -> Result<(TrayIcon, MenuHandles), RaflowError> {
        fn menu_err(e: impl std::fmt::Display) -> RaflowError {
            RaflowError::AudioCapture {
                detail: format!("failed to build tray menu: {e}"),
            }
        }
        let menu = Menu::new();
        // 版本標示（disabled）＋兩個設定開關＋設定檔捷徑＋結束（spec/settings.md §5）。
        let version_item = MenuItem::new(
            format!("raflow v{}", env!("CARGO_PKG_VERSION")),
            false,
            None,
        );
        let auto_locale = CheckMenuItem::with_id(
            MENU_ID_AUTO_LOCALE,
            "依輸入法自動切換語言",
            true,
            initial.auto_locale,
            None,
        );
        // 標示適用範圍（中文/中英混講）：en-US session 恆為 Apple 直出（其英文輸出
        // 已是母語級，Whisper zh 管線不適用；spec/whisper.md §15 locale 守門），
        // 勾選與否都不影響英文 session——選單文字必須誠實反映這一點。
        let whisper_correction = CheckMenuItem::with_id(
            MENU_ID_WHISPER_CORRECTION,
            "Whisper 智慧校正（中文/中英混講）",
            true,
            initial.whisper_correction,
            None,
        );
        let teach_correction =
            MenuItem::with_id(MENU_ID_TEACH_CORRECTION, "教 raflow 一個更正…", true, None);
        let edit_terms = MenuItem::with_id(MENU_ID_EDIT_TERMS, "編輯自訂詞彙…", true, None);
        let edit_replacements =
            MenuItem::with_id(MENU_ID_EDIT_REPLACEMENTS, "編輯取代規則…", true, None);
        let quit_item = MenuItem::with_id(QUIT_MENU_ID, "結束 raflow", true, None);

        menu.append(&version_item).map_err(menu_err)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(menu_err)?;
        menu.append(&auto_locale).map_err(menu_err)?;
        menu.append(&whisper_correction).map_err(menu_err)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(menu_err)?;
        menu.append(&teach_correction).map_err(menu_err)?;
        menu.append(&edit_terms).map_err(menu_err)?;
        menu.append(&edit_replacements).map_err(menu_err)?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(menu_err)?;
        menu.append(&quit_item).map_err(menu_err)?;

        // Idle icon 走 template（黑白剪影，由 macOS 依 menu bar 明暗 tint）；
        // Recording icon 走全彩（要顯出真正的紅色錄音點），在 set_icon 時
        // 另行 set_icon_as_template(false) 關閉 tint。
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("raflow")
            .with_icon(idle_icon)
            .with_icon_as_template(true)
            .build()
            .map_err(|e| RaflowError::AudioCapture {
                detail: format!("failed to build tray icon: {e}"),
            })?;
        Ok((
            tray,
            MenuHandles {
                auto_locale,
                whisper_correction,
            },
        ))
    }

    /// 使用者術語檔路徑（與 raflow-speech 的解析一致：env `RAFLOW_CONTEXTUAL_TERMS`
    /// 覆寫，否則 Application Support）。供 menu「編輯自訂詞彙…」開啟。
    fn contextual_terms_edit_path() -> Option<std::path::PathBuf> {
        if let Some(p) = std::env::var_os("RAFLOW_CONTEXTUAL_TERMS") {
            return Some(std::path::PathBuf::from(p));
        }
        let home = std::env::var_os("HOME")?;
        let mut p = std::path::PathBuf::from(home);
        p.push("Library");
        p.push("Application Support");
        p.push("raflow");
        p.push("contextual_terms.txt");
        Some(p)
    }

    const CONTEXTUAL_TERMS_TEMPLATE: &str = "\
# raflow 自訂術語（每行一個；# 開頭為註解）
#
# 內建已涵蓋約 120 個常用術語（Kubernetes / Docker / AWS / Terraform /
# PostgreSQL / ChatGPT…），這裡只需要放「內建沒有的、你自己常用的」詞。
#
# 最上方的詞優先進入 Whisper 修正提示（上限 30）——把最常被聽錯的放最前面。
# 改完存檔後，下次「雙擊 Cmd 開始錄音」即生效，不必重啟 app。
#
# 範例（拿掉開頭的 # 即生效）：
# Raycast
# LangChain
# 客戶專案代號
";

    const REPLACEMENTS_TEMPLATE: &str = "\
# raflow 取代規則（每行：聽錯 => 正確；# 開頭為註解）
#
# 用途：對「穩定重現的誤認」做確定性修正——同一個詞每次都被聽成同一個
# 錯法時，加一條規則一勞永逸。英文比對不分大小寫；長規則自動優先。
# 改完存檔後，下次錄音即生效，不必重啟 app。
#
# 範例（拿掉開頭的 # 即生效）：
# Teraphone => Terraform
# 阿狗CD => ArgoCD
";

    /// menu「編輯…」動作：檔案不存在先建立範本，再交給預設文字編輯器開啟。
    /// 失敗只記 log（menu 動作不可讓 app 崩潰）。
    fn open_user_config(path: Option<std::path::PathBuf>, template: &str) {
        let Some(path) = path else {
            eprintln!("! 無法解析設定檔路徑（HOME 未設？）");
            return;
        };
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Err(e) = std::fs::write(&path, template) {
                eprintln!("! 建立 {} 失敗: {e}", path.display());
                return;
            }
        }
        if let Err(e) = std::process::Command::new("open")
            .arg("-t")
            .arg(&path)
            .spawn()
        {
            eprintln!("! 開啟 {} 失敗: {e}", path.display());
        }
    }

    /// D1 menu「教 raflow 一個更正…」動作：開擷取 popover（`聽成` 下拉帶最近注入 token），使用者
    /// 按「記住」後把 `聽成 => 正確` 寫進 `replacements.txt`（下次錄音生效）；若勾「也加優先區」則把
    /// `正確` 提升到 `contextual_terms.txt` 優先區頂端。純核心（驗證／upsert／原子寫）在 raflow-input。
    /// 全程失敗只記 log，menu 動作不可讓 app 崩潰。必須在主執行緒呼叫（popover 內以 MainThreadMarker 保證）。
    /// menu「教一個更正」動作。**成功靜默**（對話框關掉＝成功，使用者選擇的 UX）；只有**出錯**
    /// 才彈原生提示（`show_notice`）：空欄位、找不到路徑、寫入失敗。取消／非主執行緒 → 無事發生。
    fn teach_correction(recent_tokens: &[String]) {
        let Some(input) = crate::correction_popover::prompt_correction(recent_tokens) else {
            return; // 取消或非主執行緒
        };
        let heard = input.heard.trim();
        let correct = input.correct.trim();
        if heard.is_empty() || correct.is_empty() {
            crate::correction_popover::show_notice("未記住", "「聽成」和「正確」都要填。");
            return;
        }
        let Some(rpath) = replacements_path() else {
            crate::correction_popover::show_notice("記住失敗", "找不到設定檔路徑（HOME 未設？）。");
            return;
        };
        if let Err(e) = upsert_replacement_file(&rpath, heard, correct) {
            report_error("! 更正寫入失敗", &e);
            crate::correction_popover::show_notice("記住失敗", &format!("寫入取代規則失敗：{e}"));
            return;
        }
        // 優先區：讀失敗（非「不存在」）不當空檔——由 upsert_contextual_priority_term_file 保證，
        // 避免覆蓋既有詞庫（Codex round-2）。失敗只提示但不算整體失敗（取代規則已存）。
        if input.add_to_priority {
            match contextual_terms_edit_path() {
                Some(cpath) => {
                    if let Err(e) = upsert_contextual_priority_term_file(&cpath, correct) {
                        report_error("! 優先區寫入失敗（取代規則已存）", &e);
                        crate::correction_popover::show_notice(
                            "已記住（優先區未加入）",
                            &format!("取代規則已存，但加入 Whisper 優先區失敗：{e}"),
                        );
                        return;
                    }
                }
                None => {
                    eprintln!("! 無法解析 contextual_terms.txt 路徑，未加入優先區（取代規則已存）");
                    crate::correction_popover::show_notice(
                        "已記住（優先區未加入）",
                        "取代規則已存，但找不到 contextual_terms.txt 路徑。",
                    );
                    return;
                }
            }
        }
        // 成功：不彈提示，對話框已關＝完成。log 供終端排查。
        eprintln!(
            "raflow: 已記住更正「{heard} => {correct}」{}（下次錄音生效）",
            if input.add_to_priority {
                "＋優先區"
            } else {
                ""
            }
        );
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

        // 使用者設定：檔案載入 + RAFLOW_ROLLING env 覆寫摺疊（spec/settings.md §3）。
        // ArcSwap 共享（憲法 4.2）：worker 的閘門/locale provider 讀、menu 開關寫。
        let initial_settings = settings::settings_path()
            .as_deref()
            .map(settings::load)
            .unwrap_or_default()
            .apply_env_override(std::env::var("RAFLOW_ROLLING").ok().as_deref());
        let user_settings: Arc<ArcSwap<Settings>> =
            Arc::new(ArcSwap::from_pointee(initial_settings));

        let (hotkey_tx, hotkey_rx) = unbounded_channel::<HotkeyEvent>();
        let (audio_tx, audio_rx) = unbounded_channel::<AudioFrame>();
        let (transcript_tx, transcript_rx) = unbounded_channel::<TranscriptUpdate>();

        let proxy_for_printer = proxy.clone();
        thread::Builder::new()
            .name("raflow-printer".into())
            .spawn(move || run_printer(transcript_rx, proxy_for_printer))
            .map_err(|e| spawn_error(format!("spawn printer thread: {e}")))?;

        let proxy_for_worker = proxy.clone();
        let settings_for_worker = user_settings.clone();
        thread::Builder::new()
            .name("raflow-worker".into())
            .spawn(move || {
                if let Err(err) = run_worker(
                    hotkey_rx,
                    audio_rx,
                    audio_tx,
                    transcript_tx,
                    proxy_for_worker,
                    settings_for_worker,
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
        // Menu 設定開關 handle：與 tray 同於 StartCause::Init 建立。
        let mut menu_handles: Option<MenuHandles> = None;
        // FloatingOverlay 必須在主執行緒建（new() 內以 MainThreadMarker 驗證）；
        // 跟 tray 一起在 StartCause::Init 建。
        let mut overlay: Option<FloatingOverlay> = None;
        // Pending hide 排程：到期後關閉浮動視窗。新的 OverlayText(Some) 來會 cancel。
        let mut overlay_hide_at: Option<Instant> = None;
        // 本次錄音 session 的 focus 狀態（在 RecordingStarted 時 query 一次定案，避免每個
        // partial 都打 AX API；spec/overlay.md §8.3-fix）。預設 false 代表「不確定」→
        // 顯示 floating panel 作為視覺安全網。
        let mut session_focused_in_text_input: bool = false;
        // D1：主執行緒持有的「最近注入英文 token」候選唯讀副本（printer 經 proxy 發布，見
        // UserEvent::RecentTokensUpdated）。只在主執行緒讀寫，不跨緒共享。供更正 popover 的下拉。
        let mut recent_tokens: Vec<String> = Vec::new();

        event_loop.run(move |event, _target, control_flow| {
            // 預設 wait；若有 pending hide 則 wait 到指定時間醒來收尾
            *control_flow = match overlay_hide_at {
                Some(at) => ControlFlow::WaitUntil(at),
                None => ControlFlow::Wait,
            };

            match event {
                Event::NewEvents(StartCause::Init) => {
                    match build_tray(idle_icon.clone(), **user_settings.load()) {
                        Ok((t, handles)) => {
                            tray = Some(t);
                            menu_handles = Some(handles);
                        }
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
                    UserEvent::RecentTokensUpdated(v) => {
                        recent_tokens = v;
                    }
                    UserEvent::MenuClick(id) => {
                        if id == QUIT_MENU_ID {
                            *control_flow = ControlFlow::Exit;
                        } else if id == MENU_ID_AUTO_LOCALE || id == MENU_ID_WHISPER_CORRECTION {
                            // CheckMenuItem 點擊時 muda 已自動翻轉勾選狀態 →
                            // 讀新狀態回寫 ArcSwap（下次錄音生效）+ 持久化到檔案。
                            if let Some(handles) = menu_handles.as_ref() {
                                let new_settings = Settings {
                                    auto_locale: handles.auto_locale.is_checked(),
                                    whisper_correction: handles
                                        .whisper_correction
                                        .is_checked(),
                                };
                                user_settings.store(Arc::new(new_settings));
                                match settings::settings_path() {
                                    Some(p) => {
                                        if let Err(e) = settings::save(&p, new_settings) {
                                            eprintln!(
                                                "! settings 寫入失敗（in-memory 已生效）: {e}"
                                            );
                                        }
                                    }
                                    None => eprintln!(
                                        "! settings 路徑無法解析（HOME 未設？），變更僅本次執行有效"
                                    ),
                                }
                                eprintln!(
                                    "raflow: 設定更新 — 自動語言切換={} / Whisper 智慧校正={}（下次錄音生效）",
                                    new_settings.auto_locale, new_settings.whisper_correction
                                );
                            }
                        } else if id == MENU_ID_EDIT_TERMS {
                            open_user_config(
                                contextual_terms_edit_path(),
                                CONTEXTUAL_TERMS_TEMPLATE,
                            );
                        } else if id == MENU_ID_EDIT_REPLACEMENTS {
                            open_user_config(replacements_path(), REPLACEMENTS_TEMPLATE);
                        } else if id == MENU_ID_TEACH_CORRECTION {
                            // 成功靜默（對話框關掉＝完成）；出錯由 teach_correction 內部彈原生提示。
                            teach_correction(&recent_tokens);
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
                RaflowError::InvalidReplacement {
                    detail: "heard 不可含換行".into(),
                },
                RaflowError::ConfigWrite {
                    path: "/x/replacements.txt".into(),
                    source: std::io::Error::other("disk full"),
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
