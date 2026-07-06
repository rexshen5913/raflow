#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptUpdate {
    /// Worker 進入 Recording 狀態的訊號，給 printer 用來重置 streaming inject 狀態
    /// （`last_partial`），避免上一次 session 沒收到 Final 時殘留導致下一次 session
    /// 第一個 partial 算出錯誤的 backspace。詳見 docs/spec/input.md §3。
    SessionStarted,
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
    fn session_started_is_distinct_unit_variant() {
        let a = TranscriptUpdate::SessionStarted;
        let b = TranscriptUpdate::SessionStarted;
        assert_eq!(a, b);
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
