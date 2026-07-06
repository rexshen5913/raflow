//! raflow-app 協調邏輯：把 hotkey、audio、speech 串起來，採 toggle 語意。
//!
//! 本 lib 只負責純協調（無 I/O 副作用）；真實模組接線由 bin 側（main.rs）完成。
//! 完整範圍見 `docs/spec/app.md`。

use raflow_core::{AudioFrame, HotkeyEvent, RaflowError, TranscriptUpdate};
use raflow_speech::{Session, SpeechBackend};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppState {
    Idle,
    Recording,
}

/// 對 hotkey 處理後，由 caller 需負責執行的副作用指示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    StartRecording,
    StopRecording,
}

pub struct App<B: SpeechBackend> {
    session: Session<B>,
    state: AppState,
    /// 每次進入 Recording 前呼叫一次，決定該 session 的 speech locale。
    /// 真實接線（main.rs）注入「讀當前輸入法」的實作；測試注入純閉包。
    /// 抽成 provider 是為了讓本 lib 維持「無 I/O 副作用」（OS FFI 留在 bin 側）。
    locale_provider: Box<dyn Fn() -> String + Send>,
    /// preferred locale 的 recognizer 不可用（`SpeechUnavailable`）時退回的 locale。
    /// 保證「最差退回原本行為」：例如輸入法是英文但該機 en-US Speech 不可用時，仍以此
    /// 啟動而非讓錄音整個失敗。
    fallback_locale: String,
    transcript_tx: UnboundedSender<TranscriptUpdate>,
}

impl<B: SpeechBackend> App<B> {
    /// 固定 locale 版本：語意等同注入一個永遠回傳 `locale` 的 provider，且 fallback 同值。
    pub fn new(
        backend: B,
        locale: String,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
    ) -> Self {
        let fallback = locale.clone();
        Self::with_locale_provider(
            backend,
            Box::new(move || locale.clone()),
            fallback,
            transcript_tx,
        )
    }

    /// 動態 locale 版本：`locale_provider` 於每次 Idle→Recording 時被呼叫；preferred
    /// locale 不可用時退回 `fallback_locale`。
    pub fn with_locale_provider(
        backend: B,
        locale_provider: Box<dyn Fn() -> String + Send>,
        fallback_locale: String,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
    ) -> Self {
        Self {
            session: Session::new(backend),
            state: AppState::Idle,
            locale_provider,
            fallback_locale,
            transcript_tx,
        }
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.state, AppState::Recording)
    }

    pub fn on_hotkey(&mut self, event: HotkeyEvent) -> Result<Transition, RaflowError> {
        match (self.state, event) {
            (AppState::Idle, HotkeyEvent::Pressed) => {
                // 每次開始錄音當下決定 locale（例如讀當前輸入法）。
                let preferred = (self.locale_provider)();
                match self.session.start(&preferred, self.transcript_tx.clone()) {
                    Ok(()) => {}
                    // preferred 語言的 recognizer 不可用 → 退回 fallback locale（最差維持
                    // 原本行為，不讓錄音整個失敗）。fallback 若也失敗則往上拋，App 留在 Idle。
                    Err(RaflowError::SpeechUnavailable { .. })
                        if preferred != self.fallback_locale =>
                    {
                        self.session
                            .start(&self.fallback_locale, self.transcript_tx.clone())?;
                    }
                    Err(e) => return Err(e),
                }
                self.state = AppState::Recording;
                // Reset cue 給 printer：避免上次 session 沒收到 Final 時 last_partial 殘留
                // 導致下一次 session 第一個 partial 算出錯誤的 backspace。
                // 詳見 docs/spec/input.md §3。Send 失敗代表 receiver 已關，整條 pipeline
                // 都壞了，這裡靜默忽略，由後續 transcript send 自然炸出。
                let _ = self.transcript_tx.send(TranscriptUpdate::SessionStarted);
                Ok(Transition::StartRecording)
            }
            (AppState::Recording, HotkeyEvent::Pressed) => {
                // 句級滾動收尾 flush：必須在 stop 前（session 仍 Streaming 才會轉發）。
                // 非滾動 backend 的 rolling_tick 為 no-op，故對現行行為無影響（ADR-0006 §8.7.2）。
                self.session.rolling_tick(true)?;
                self.session.stop()?;
                self.state = AppState::Idle;
                Ok(Transition::StopRecording)
            }
            (_, HotkeyEvent::Released) => Ok(Transition::None),
        }
    }

    pub fn on_audio_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError> {
        self.session.push_frame(frame)
    }

    /// Phase 2 句級滾動 tick：由 caller（main.rs 計時器）週期呼叫，只在錄音中轉發給 session。
    /// `is_final=true` 保留給停止收尾（實際上 stop 流程已在 `on_hotkey` 內 flush，此參數供
    /// 未來擴充/測試對稱）。非滾動 backend 收到後 no-op。
    pub fn on_rolling_tick(&mut self, is_final: bool) -> Result<(), RaflowError> {
        self.session.rolling_tick(is_final)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use tokio::sync::mpsc::unbounded_channel;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum FakeEvent {
        Started(String),
        Frame(usize),
        Stopped,
        RollingTick(bool),
    }

    #[derive(Default)]
    struct FakeInner {
        events: Vec<FakeEvent>,
        start_error: Option<RaflowError>,
        /// 這些 locale 的 start 回 `SpeechUnavailable`（模擬該語言 recognizer 不可用）。
        unavailable: Vec<String>,
        /// 每次 start 嘗試的 locale（含失敗），用來驗證 fallback 是否真的先試 preferred。
        attempts: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct FakeBackend {
        inner: Rc<RefCell<FakeInner>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self::default()
        }

        fn with_start_error(err: RaflowError) -> Self {
            let fb = Self::new();
            fb.inner.borrow_mut().start_error = Some(err);
            fb
        }

        /// 指定哪些 locale 的 start 應回 `SpeechUnavailable`。
        fn with_unavailable(locales: &[&str]) -> Self {
            let fb = Self::new();
            fb.inner.borrow_mut().unavailable = locales.iter().map(|l| l.to_string()).collect();
            fb
        }

        fn events(&self) -> Vec<FakeEvent> {
            self.inner.borrow().events.clone()
        }

        fn attempts(&self) -> Vec<String> {
            self.inner.borrow().attempts.clone()
        }
    }

    impl SpeechBackend for FakeBackend {
        fn start(
            &mut self,
            locale: &str,
            _tx: UnboundedSender<TranscriptUpdate>,
        ) -> Result<(), RaflowError> {
            let mut inner = self.inner.borrow_mut();
            inner.attempts.push(locale.to_string());
            if let Some(err) = inner.start_error.take() {
                return Err(err);
            }
            if inner.unavailable.iter().any(|l| l == locale) {
                return Err(RaflowError::SpeechUnavailable {
                    locale: locale.to_string(),
                });
            }
            inner.events.push(FakeEvent::Started(locale.to_string()));
            Ok(())
        }

        fn push_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError> {
            self.inner
                .borrow_mut()
                .events
                .push(FakeEvent::Frame(frame.pcm.len()));
            Ok(())
        }

        fn stop(&mut self) -> Result<(), RaflowError> {
            self.inner.borrow_mut().events.push(FakeEvent::Stopped);
            Ok(())
        }

        fn rolling_tick(&mut self, is_final: bool) -> Result<(), RaflowError> {
            self.inner
                .borrow_mut()
                .events
                .push(FakeEvent::RollingTick(is_final));
            Ok(())
        }
    }

    fn sample_frame() -> AudioFrame {
        AudioFrame {
            pcm: vec![0; 320],
            sample_rate: 16_000,
        }
    }

    fn fresh_app(fake: FakeBackend) -> App<FakeBackend> {
        let (tx, _rx) = unbounded_channel();
        App::new(fake, "zh-TW".to_string(), tx)
    }

    #[test]
    fn initial_state_is_idle() {
        let app = fresh_app(FakeBackend::new());
        assert!(!app.is_recording());
    }

    #[test]
    fn pressed_toggles_between_idle_and_recording() {
        let fake = FakeBackend::new();
        let mut app = fresh_app(fake.clone());

        let cases = [
            (HotkeyEvent::Pressed, Transition::StartRecording, true),
            (HotkeyEvent::Pressed, Transition::StopRecording, false),
            (HotkeyEvent::Pressed, Transition::StartRecording, true),
            (HotkeyEvent::Pressed, Transition::StopRecording, false),
        ];
        for (event, expected_transition, expected_recording) in cases {
            let t = app.on_hotkey(event).expect("hotkey handling failed");
            assert_eq!(t, expected_transition);
            assert_eq!(app.is_recording(), expected_recording);
        }

        assert_eq!(
            fake.events(),
            vec![
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::RollingTick(true),
                FakeEvent::Stopped,
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::RollingTick(true),
                FakeEvent::Stopped,
            ],
        );
    }

    #[test]
    fn released_is_ignored_in_both_states() {
        let fake = FakeBackend::new();
        let mut app = fresh_app(fake.clone());

        let released_idle = app.on_hotkey(HotkeyEvent::Released).unwrap();
        assert_eq!(released_idle, Transition::None);
        assert!(!app.is_recording());

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        let released_recording = app.on_hotkey(HotkeyEvent::Released).unwrap();
        assert_eq!(released_recording, Transition::None);
        assert!(app.is_recording(), "Released must not stop recording");

        assert_eq!(fake.events(), vec![FakeEvent::Started("zh-TW".into())]);
    }

    #[test]
    fn audio_frames_forward_only_when_recording() {
        let fake = FakeBackend::new();
        let mut app = fresh_app(fake.clone());
        let frame = sample_frame();

        app.on_audio_frame(&frame).unwrap();

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        app.on_audio_frame(&frame).unwrap();
        app.on_audio_frame(&frame).unwrap();

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        app.on_audio_frame(&frame).unwrap();

        assert_eq!(
            fake.events(),
            vec![
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::Frame(320),
                FakeEvent::Frame(320),
                FakeEvent::RollingTick(true),
                FakeEvent::Stopped,
            ],
        );
    }

    #[test]
    fn start_error_leaves_app_idle_and_allows_retry() {
        let fake = FakeBackend::with_start_error(RaflowError::SpeechUnavailable {
            locale: "zh-TW".into(),
        });
        let mut app = fresh_app(fake.clone());

        let err = app.on_hotkey(HotkeyEvent::Pressed).unwrap_err();
        assert!(matches!(err, RaflowError::SpeechUnavailable { .. }));
        assert!(!app.is_recording(), "failed start must keep us in Idle");

        let retry = app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        assert_eq!(retry, Transition::StartRecording);
        assert!(app.is_recording());

        assert_eq!(fake.events(), vec![FakeEvent::Started("zh-TW".into())]);
    }

    /// Idle → Recording 必須送 SessionStarted；Recording → Idle 與 ignored Released 不送。
    /// 這是 printer 重置 `last_partial` 的唯一 cue，避免跨 session 殘留（spec/input.md §3）。
    #[test]
    fn session_started_emitted_only_on_start_recording() {
        let (tx, mut rx) = unbounded_channel::<TranscriptUpdate>();
        let mut app = App::new(FakeBackend::new(), "zh-TW".into(), tx);

        app.on_hotkey(HotkeyEvent::Released).unwrap();
        assert!(rx.try_recv().is_err(), "Released in Idle must not emit");

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        assert_eq!(
            rx.try_recv().ok(),
            Some(TranscriptUpdate::SessionStarted),
            "Idle → Recording must emit SessionStarted exactly once",
        );
        assert!(rx.try_recv().is_err(), "no extra emission");

        app.on_hotkey(HotkeyEvent::Released).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "Released in Recording must not emit"
        );

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        assert!(
            rx.try_recv().is_err(),
            "Recording → Idle must NOT emit SessionStarted",
        );

        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        assert_eq!(
            rx.try_recv().ok(),
            Some(TranscriptUpdate::SessionStarted),
            "second Idle → Recording must emit SessionStarted again",
        );
    }

    /// `session.start` 失敗時，App 仍在 Idle → 不可送 SessionStarted（否則 printer 會
    /// 對著沒在錄音的狀態 reset，後續若 retry 成功才送，順序一致）。
    #[test]
    fn session_started_not_emitted_when_start_fails() {
        let fake = FakeBackend::with_start_error(RaflowError::SpeechUnavailable {
            locale: "zh-TW".into(),
        });
        let (tx, mut rx) = unbounded_channel::<TranscriptUpdate>();
        let mut app = App::new(fake, "zh-TW".into(), tx);

        let _ = app.on_hotkey(HotkeyEvent::Pressed).unwrap_err();
        assert!(
            rx.try_recv().is_err(),
            "failed StartRecording must not emit SessionStarted",
        );

        // retry 成功後才該送
        app.on_hotkey(HotkeyEvent::Pressed).unwrap();
        assert_eq!(rx.try_recv().ok(), Some(TranscriptUpdate::SessionStarted));
    }

    /// 停止錄音時，必須先送一次 `rolling_tick(true)` 收尾 flush，**再** `stop`（ADR-0006 §8.7.2）。
    /// 順序關鍵：flush 必須在 session 仍 Streaming 時發生，否則 backend 收不到。
    #[test]
    fn stop_flushes_rolling_before_stopping() {
        let fake = FakeBackend::new();
        let mut app = fresh_app(fake.clone());

        app.on_hotkey(HotkeyEvent::Pressed).unwrap(); // start
        app.on_hotkey(HotkeyEvent::Pressed).unwrap(); // stop → flush then stop

        assert_eq!(
            fake.events(),
            vec![
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::RollingTick(true),
                FakeEvent::Stopped,
            ],
            "stop 前必須先 rolling_tick(true)",
        );
    }

    /// 週期性 `on_rolling_tick(false)` 只在錄音中轉發（比照 audio frame）。
    #[test]
    fn periodic_rolling_tick_forwards_only_when_recording() {
        let fake = FakeBackend::new();
        let mut app = fresh_app(fake.clone());

        app.on_rolling_tick(false).unwrap(); // idle → 丟棄
        app.on_hotkey(HotkeyEvent::Pressed).unwrap(); // start
        app.on_rolling_tick(false).unwrap(); // recording → 轉發

        let ticks: Vec<_> = fake
            .events()
            .into_iter()
            .filter(|e| matches!(e, FakeEvent::RollingTick(_)))
            .collect();
        assert_eq!(
            ticks,
            vec![FakeEvent::RollingTick(false)],
            "idle tick 不可轉發"
        );
    }

    /// 注入的 locale_provider 回傳值必須原封傳給 backend.start（動態選 locale 的基礎）。
    #[test]
    fn locale_provider_value_is_passed_to_backend_start() -> Result<(), RaflowError> {
        let fake = FakeBackend::new();
        let (tx, _rx) = unbounded_channel();
        let mut app = App::with_locale_provider(
            fake.clone(),
            Box::new(|| "en-US".to_string()),
            "zh-TW".to_string(),
            tx,
        );

        app.on_hotkey(HotkeyEvent::Pressed)?;

        assert_eq!(fake.events(), vec![FakeEvent::Started("en-US".into())]);
        Ok(())
    }

    /// provider 必須在**每次** Idle→Recording 重讀（不可只讀一次快取）——這是「錄音當下
    /// 依輸入法決定 locale」的關鍵；輸入法在兩次錄音間切換時要跟得上。
    #[test]
    fn locale_provider_is_read_on_each_start() -> Result<(), RaflowError> {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_in_provider = calls.clone();
        let fake = FakeBackend::new();
        let (tx, _rx) = unbounded_channel();
        let mut app = App::with_locale_provider(
            fake.clone(),
            Box::new(move || {
                let n = calls_in_provider.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    "zh-TW".to_string()
                } else {
                    "en-US".to_string()
                }
            }),
            "zh-TW".to_string(),
            tx,
        );

        app.on_hotkey(HotkeyEvent::Pressed)?; // start → zh-TW
        app.on_hotkey(HotkeyEvent::Pressed)?; // stop
        app.on_hotkey(HotkeyEvent::Pressed)?; // start → en-US（切換後）

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "provider read once per start"
        );
        assert_eq!(
            fake.events(),
            vec![
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::RollingTick(true),
                FakeEvent::Stopped,
                FakeEvent::Started("en-US".into()),
            ],
        );
        Ok(())
    }

    /// preferred locale 的 recognizer 不可用時，必須退回 fallback locale 並仍能開始錄音
    /// （最差退回原本行為）。且必須是「先試 preferred，失敗才試 fallback」。
    #[test]
    fn falls_back_to_fallback_locale_when_preferred_unavailable() -> Result<(), RaflowError> {
        let fake = FakeBackend::with_unavailable(&["en-US"]);
        let (tx, _rx) = unbounded_channel();
        let mut app = App::with_locale_provider(
            fake.clone(),
            Box::new(|| "en-US".to_string()),
            "zh-TW".to_string(),
            tx,
        );

        let t = app.on_hotkey(HotkeyEvent::Pressed)?;

        assert_eq!(t, Transition::StartRecording);
        assert!(app.is_recording(), "fallback 成功後應進入 Recording");
        assert_eq!(
            fake.attempts(),
            vec!["en-US".to_string(), "zh-TW".to_string()],
            "必須先試 preferred(en-US) 再退回 fallback(zh-TW)",
        );
        assert_eq!(
            fake.events(),
            vec![FakeEvent::Started("zh-TW".into())],
            "只有 fallback 成功啟動",
        );
        Ok(())
    }

    /// preferred 與 fallback 皆不可用 → 錯誤往上拋，App 留在 Idle，且不送 SessionStarted。
    #[test]
    fn start_fails_when_both_preferred_and_fallback_unavailable() {
        let fake = FakeBackend::with_unavailable(&["en-US", "zh-TW"]);
        let (tx, mut rx) = unbounded_channel::<TranscriptUpdate>();
        let mut app = App::with_locale_provider(
            fake.clone(),
            Box::new(|| "en-US".to_string()),
            "zh-TW".to_string(),
            tx,
        );

        let result = app.on_hotkey(HotkeyEvent::Pressed);

        assert!(
            matches!(result, Err(RaflowError::SpeechUnavailable { .. })),
            "expected SpeechUnavailable, got {result:?}",
        );
        assert!(!app.is_recording(), "兩者皆失敗必須留在 Idle");
        assert_eq!(
            fake.attempts(),
            vec!["en-US".to_string(), "zh-TW".to_string()],
            "兩個 locale 都試過",
        );
        assert!(rx.try_recv().is_err(), "完全失敗不可送 SessionStarted",);
    }
}
