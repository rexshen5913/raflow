use raflow_core::{AudioFrame, RaflowError, TranscriptUpdate};
use tokio::sync::mpsc::UnboundedSender;

use crate::backend::SpeechBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Streaming,
}

pub struct Session<B: SpeechBackend> {
    backend: B,
    state: State,
}

impl<B: SpeechBackend> Session<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: State::Idle,
        }
    }

    pub fn start(
        &mut self,
        locale: &str,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
    ) -> Result<(), RaflowError> {
        if self.state == State::Streaming {
            return Err(RaflowError::SpeechBusy);
        }
        self.backend.start(locale, transcript_tx)?;
        self.state = State::Streaming;
        Ok(())
    }

    pub fn push_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError> {
        if self.state != State::Streaming {
            return Ok(());
        }
        self.backend.push_frame(frame)
    }

    pub fn stop(&mut self) -> Result<(), RaflowError> {
        if self.state == State::Idle {
            return Ok(());
        }
        self.backend.stop()?;
        self.state = State::Idle;
        Ok(())
    }

    /// Phase 2 句級滾動 tick：只在 Streaming 時轉發給 backend（比照 [`push_frame`](Self::push_frame)）。
    /// Idle 時丟棄，避免對已停止 session 觸發滾動校正。
    pub fn rolling_tick(&mut self, is_final: bool) -> Result<(), RaflowError> {
        if self.state != State::Streaming {
            return Ok(());
        }
        self.backend.rolling_tick(is_final)
    }

    /// 本 session 是否為句級滾動（`start` 後有效）。委派 backend；供 Edit Guard 判定啟用。
    pub fn is_rolling(&self) -> bool {
        self.backend.session_rolling()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use tokio::sync::mpsc::unbounded_channel;

    #[derive(Debug, PartialEq, Eq)]
    enum FakeEvent {
        Started(String),
        Frame(usize),
        Stopped,
        RollingTick(bool),
    }

    struct FakeBackend {
        events: RefCell<Vec<FakeEvent>>,
        start_error: RefCell<Option<RaflowError>>,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                events: RefCell::new(Vec::new()),
                start_error: RefCell::new(None),
            }
        }

        fn with_start_error(err: RaflowError) -> Self {
            let backend = Self::new();
            *backend.start_error.borrow_mut() = Some(err);
            backend
        }

        fn events(&self) -> Vec<FakeEvent> {
            self.events.borrow().iter().cloned().collect()
        }
    }

    impl Clone for FakeEvent {
        fn clone(&self) -> Self {
            match self {
                FakeEvent::Started(s) => FakeEvent::Started(s.clone()),
                FakeEvent::Frame(n) => FakeEvent::Frame(*n),
                FakeEvent::Stopped => FakeEvent::Stopped,
                FakeEvent::RollingTick(f) => FakeEvent::RollingTick(*f),
            }
        }
    }

    impl SpeechBackend for FakeBackend {
        fn start(
            &mut self,
            locale: &str,
            _tx: UnboundedSender<TranscriptUpdate>,
        ) -> Result<(), RaflowError> {
            if let Some(err) = self.start_error.borrow_mut().take() {
                return Err(err);
            }
            self.events
                .borrow_mut()
                .push(FakeEvent::Started(locale.to_string()));
            Ok(())
        }

        fn push_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError> {
            self.events
                .borrow_mut()
                .push(FakeEvent::Frame(frame.pcm.len()));
            Ok(())
        }

        fn stop(&mut self) -> Result<(), RaflowError> {
            self.events.borrow_mut().push(FakeEvent::Stopped);
            Ok(())
        }

        fn rolling_tick(&mut self, is_final: bool) -> Result<(), RaflowError> {
            self.events
                .borrow_mut()
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

    fn fresh_tx() -> UnboundedSender<TranscriptUpdate> {
        unbounded_channel().0
    }

    #[test]
    fn start_transitions_idle_to_streaming_and_calls_backend() {
        let mut session = Session::new(FakeBackend::new());
        assert!(session.start("zh-TW", fresh_tx()).is_ok());
        assert_eq!(session.state, State::Streaming);
        assert_eq!(
            session.backend.events(),
            vec![FakeEvent::Started("zh-TW".into())]
        );
    }

    #[test]
    fn double_start_returns_busy_without_touching_backend() {
        let mut session = Session::new(FakeBackend::new());
        assert!(session.start("zh-TW", fresh_tx()).is_ok());
        let result = session.start("zh-TW", fresh_tx());
        assert!(matches!(result, Err(RaflowError::SpeechBusy)));
        assert_eq!(
            session.backend.events(),
            vec![FakeEvent::Started("zh-TW".into())]
        );
    }

    #[test]
    fn start_error_leaves_session_idle() {
        let mut session = Session::new(FakeBackend::with_start_error(
            RaflowError::SpeechUnavailable {
                locale: "zh-TW".into(),
            },
        ));
        let result = session.start("zh-TW", fresh_tx());
        assert!(matches!(result, Err(RaflowError::SpeechUnavailable { .. })));
        assert_eq!(session.state, State::Idle);
    }

    #[test]
    fn push_frame_forwards_only_when_streaming() {
        let cases = [
            (false, 0, "idle drops frames silently"),
            (true, 1, "streaming forwards to backend"),
        ];
        for (start_first, expected_forwards, label) in cases {
            let mut session = Session::new(FakeBackend::new());
            if start_first {
                assert!(session.start("zh-TW", fresh_tx()).is_ok(), "{label}");
            }
            let frame = sample_frame();
            assert!(session.push_frame(&frame).is_ok(), "{label}");

            let frame_count = session
                .backend
                .events()
                .iter()
                .filter(|ev| matches!(ev, FakeEvent::Frame(_)))
                .count();
            assert_eq!(frame_count, expected_forwards, "{label}");
        }
    }

    #[test]
    fn stop_transitions_to_idle_and_permits_restart() {
        let mut session = Session::new(FakeBackend::new());
        assert!(session.start("zh-TW", fresh_tx()).is_ok());
        assert!(session.stop().is_ok());
        assert_eq!(session.state, State::Idle);

        assert!(session.start("zh-TW", fresh_tx()).is_ok());
        assert_eq!(session.state, State::Streaming);
        assert_eq!(
            session.backend.events(),
            vec![
                FakeEvent::Started("zh-TW".into()),
                FakeEvent::Stopped,
                FakeEvent::Started("zh-TW".into()),
            ]
        );
    }

    #[test]
    fn rolling_tick_forwards_only_when_streaming() {
        let cases = [
            (false, 0, "idle drops rolling tick"),
            (true, 1, "streaming forwards rolling tick"),
        ];
        for (start_first, expected_forwards, label) in cases {
            let mut session = Session::new(FakeBackend::new());
            if start_first {
                assert!(session.start("zh-TW", fresh_tx()).is_ok(), "{label}");
            }
            assert!(session.rolling_tick(false).is_ok(), "{label}");
            assert!(session.rolling_tick(true).is_ok(), "{label}");

            let tick_count = session
                .backend
                .events()
                .iter()
                .filter(|ev| matches!(ev, FakeEvent::RollingTick(_)))
                .count();
            assert_eq!(tick_count, expected_forwards * 2, "{label}");
        }
    }

    #[test]
    fn stop_in_idle_is_noop() {
        let mut session = Session::new(FakeBackend::new());
        assert!(session.stop().is_ok());
        assert_eq!(session.state, State::Idle);
        assert_eq!(session.backend.events(), vec![]);
    }
}
