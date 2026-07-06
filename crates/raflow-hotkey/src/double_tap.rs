//! 雙擊 Cmd 偵測純邏輯狀態機（不含任何 NSEvent / unsafe / FFI）。
//!
//! 設計說明見 ADR-0004。事件來源（NSEvent global monitor）由 `nsevent_monitor.rs`
//! 注入，本模組只負責「給定一系列 (event, instant) 是否構成有效雙擊」。
//!
//! 規則：
//! 1. 兩次 Cmd 按下時間差 ≤ `window` → 觸發
//! 2. 中間若有任何其他事件（其他 modifier、其他 key）→ reset
//! 3. 觸發於第二次 Cmd 按下時點（responsiveness 優於釋放）

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    FirstPress { pressed_at: Instant },
    FirstRelease { released_at: Instant },
}

pub struct DoubleTapDetector {
    state: State,
    window: Duration,
}

impl DoubleTapDetector {
    pub fn new(window: Duration) -> Self {
        Self {
            state: State::Idle,
            window,
        }
    }

    /// Cmd 被按下；回傳是否構成有效雙擊（caller 應 fire `HotkeyEvent::Pressed`）。
    pub fn on_cmd_down(&mut self, now: Instant) -> bool {
        match self.state {
            State::Idle | State::FirstPress { .. } => {
                // 第一次 down，或多重 down（auto-repeat 等異常）→ 視為新序列起點
                self.state = State::FirstPress { pressed_at: now };
                false
            }
            State::FirstRelease { released_at } => {
                if now.duration_since(released_at) <= self.window {
                    self.state = State::Idle;
                    true
                } else {
                    self.state = State::FirstPress { pressed_at: now };
                    false
                }
            }
        }
    }

    pub fn on_cmd_up(&mut self, now: Instant) {
        if let State::FirstPress { .. } = self.state {
            self.state = State::FirstRelease { released_at: now };
        }
    }

    /// 任何非 Cmd 的事件（其他 modifier 變動、其他 keystroke）→ 序列被打斷。
    pub fn on_other_event(&mut self) {
        if !matches!(self.state, State::Idle) {
            self.state = State::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> DoubleTapDetector {
        DoubleTapDetector::new(Duration::from_millis(300))
    }

    fn t(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn double_tap_within_window_fires() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_cmd_up(t(t0, 50));
        assert!(d.on_cmd_down(t(t0, 200)), "should fire (gap 150ms)");
    }

    #[test]
    fn double_tap_outside_window_does_not_fire() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_cmd_up(t(t0, 50));
        // gap 400ms > 300ms window
        assert!(
            !d.on_cmd_down(t(t0, 450)),
            "should not fire (gap exceeds window)"
        );
    }

    #[test]
    fn other_key_during_first_press_breaks_sequence() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_other_event(); // e.g. Cmd+S — S 被按
        d.on_cmd_up(t(t0, 50));
        assert!(
            !d.on_cmd_down(t(t0, 100)),
            "Cmd combo should not be misread as double-tap"
        );
    }

    #[test]
    fn other_key_during_first_release_breaks_sequence() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_cmd_up(t(t0, 50));
        d.on_other_event(); // 中間敲了別的鍵
        assert!(
            !d.on_cmd_down(t(t0, 200)),
            "interleaved keystroke should reset detector"
        );
    }

    #[test]
    fn triple_tap_fires_once() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_cmd_up(t(t0, 30));
        assert!(d.on_cmd_down(t(t0, 100)), "2nd down → fire");
        d.on_cmd_up(t(t0, 130));
        assert!(
            !d.on_cmd_down(t(t0, 200)),
            "3rd down should start new sequence (not fire alone)"
        );
    }

    #[test]
    fn quadruple_tap_fires_twice() {
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        d.on_cmd_up(t(t0, 30));
        assert!(d.on_cmd_down(t(t0, 100)), "2nd down → fire #1");
        d.on_cmd_up(t(t0, 130));
        assert!(!d.on_cmd_down(t(t0, 200)), "3rd down restarts sequence");
        d.on_cmd_up(t(t0, 230));
        assert!(d.on_cmd_down(t(t0, 300)), "4th down → fire #2");
    }

    #[test]
    fn cmd_up_without_prior_down_is_noop() {
        let mut d = detector();
        let t0 = Instant::now();

        d.on_cmd_up(t(t0, 0));
        // 接下來的雙擊應正常運作
        assert!(!d.on_cmd_down(t(t0, 100)));
        d.on_cmd_up(t(t0, 130));
        assert!(d.on_cmd_down(t(t0, 200)));
    }

    #[test]
    fn double_down_without_up_resets_press_timestamp() {
        // 異常情況：兩次 down 中間沒 up（理論上不會發生，但 NSEvent 偶爾會掉 release event）
        let mut d = detector();
        let t0 = Instant::now();

        assert!(!d.on_cmd_down(t(t0, 0)));
        assert!(
            !d.on_cmd_down(t(t0, 50)),
            "no Cmd-up between → still not a valid double-tap"
        );
        d.on_cmd_up(t(t0, 100));
        assert!(
            d.on_cmd_down(t(t0, 200)),
            "subsequent valid double-tap should fire"
        );
    }
}
