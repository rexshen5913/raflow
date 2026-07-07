#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptUpdate {
    /// Worker 進入 Recording 狀態的訊號，給 printer 用來重置 streaming inject 狀態
    /// （`last_partial`），避免上一次 session 沒收到 Final 時殘留導致下一次 session
    /// 第一個 partial 算出錯誤的 backspace。詳見 docs/spec/input.md §3。
    ///
    /// `rolling`：本 session 是否為句級滾動（會產生中途 `PhraseFinal` 段界）。Edit Guard
    /// 只在 rolling session 啟用——非滾動 session 沒有中途段界可作恢復錨點，若凍結會卡到
    /// 錄音結束（且非滾動沒有「改中途定稿詞」情境）。詳見 docs/spec/input.md §7f。
    SessionStarted { rolling: bool },
    Partial(String),
    /// 句級滾動校正：這句 Whisper 定稿 → printer 對齊後鎖定，之後不再更動。
    /// `Partial` 只代表**當前未定稿句**；`PhraseFinal` 代表**該句定稿並鎖定**。
    /// 詳見 ADR-0006 §2.4 與 docs/spec/input.md。
    PhraseFinal(String),
    Final(String),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_preserve_their_payload() {
        let cases = [
            TranscriptUpdate::Partial("你好".into()),
            TranscriptUpdate::Final("你好世界".into()),
            TranscriptUpdate::Error("network offline".into()),
        ];
        for original in &cases {
            let clone = original.clone();
            assert_eq!(&clone, original);
        }
    }

    #[test]
    fn partial_and_final_with_same_text_are_distinct() {
        let a = TranscriptUpdate::Partial("hi".into());
        let b = TranscriptUpdate::Final("hi".into());
        assert_ne!(a, b);
    }

    #[test]
    fn session_started_carries_rolling_flag() {
        let a = TranscriptUpdate::SessionStarted { rolling: true };
        assert_eq!(a.clone(), a);
        assert_ne!(
            TranscriptUpdate::SessionStarted { rolling: true },
            TranscriptUpdate::SessionStarted { rolling: false },
        );
        assert_ne!(a, TranscriptUpdate::Partial(String::new()));
        assert_ne!(a, TranscriptUpdate::Final(String::new()));
    }

    /// ADR-0006 §2.4：`PhraseFinal` 是獨立變體，與同文字的 `Partial` / `Final` 皆不相等，
    /// 且保留 payload。printer 據此把「句級鎖定」與「session 收尾」分開處理。
    #[test]
    fn phrase_final_is_distinct_from_partial_and_final() {
        let pf = TranscriptUpdate::PhraseFinal("你好，世界".into());
        assert_eq!(pf.clone(), pf);
        assert_ne!(pf, TranscriptUpdate::Partial("你好，世界".into()));
        assert_ne!(pf, TranscriptUpdate::Final("你好，世界".into()));
        match pf {
            TranscriptUpdate::PhraseFinal(t) => assert_eq!(t, "你好，世界"),
            other => panic!("expected PhraseFinal, got {other:?}"),
        }
    }
}
