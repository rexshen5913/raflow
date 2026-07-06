//! raflow-input：把 partial / final transcript 注入到使用者目前 focus 的輸入框，
//! 並於 Final 時同時複製到系統剪貼簿作 fallback。
//!
//! 完整範圍見 `docs/spec/input.md`。

use raflow_core::RaflowError;

/// `stream_update` 的差分結果：先 backspace 幾次，再 type 哪段 suffix。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamDiff {
    pub backspace: usize,
    pub append: String,
}

/// 計算把 `prev` 轉成 `new` 所需的最小 backspace + append 動作。
///
/// - 字元以 [`char`]（Unicode scalar value）為單位，與 `enigo` 在 macOS 上一個
///   keystroke 等同刪除一個字元的行為對齊（含中日韓）。
/// - MVP 不採 grapheme cluster：Apple Speech zh-TW 不會輸出 ZWJ 組合 emoji。
pub fn compute_stream_diff(prev: &str, new: &str) -> StreamDiff {
    let prefix_chars = prev
        .chars()
        .zip(new.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let prev_len = prev.chars().count();
    let backspace = prev_len - prefix_chars;
    let append: String = new.chars().skip(prefix_chars).collect();
    StreamDiff { backspace, append }
}

/// 使用者自訂「取代對照表」：一組 `(聽錯的文字, 正確文字)`。用來確定性修正 Apple/Whisper
/// 一直認錯的領域術語（如「阿狗CD」→「ArgoCD」）——contextualStrings 是軟性偏好、救不了發音
/// 撞到常見中文詞的術語，這層是輸出後的硬性字串取代，100% 生效。詳見 `docs/spec/input.md`。
pub type Replacements = Vec<(String, String)>;

/// 解析取代對照檔內容：每行 `聽錯 => 正確`；`#` 開頭為註解、空行略過；兩側 trim；缺 `=>`
/// 或任一側空 → 略過該行。
///
/// **自動依左邊（`from`）長度遞減排序**：長／精確的規則一定先套，使用者不必
/// 自己顧檔案順序（避免「短規則先把字拆掉、長規則就配不到」，如「狗」先於「阿狗CD」）。
/// 等長者維持檔案原順序（stable sort）。
pub fn parse_replacements(contents: &str) -> Replacements {
    let mut rules: Replacements = contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (from, to) = line.split_once("=>")?;
            let (from, to) = (from.trim(), to.trim());
            if from.is_empty() || to.is_empty() {
                return None;
            }
            Some((from.to_string(), to.to_string()))
        })
        .collect();
    rules.sort_by(|a, b| b.0.chars().count().cmp(&a.0.chars().count()));
    rules
}

/// 依序對 `text` 套用每組取代，**ASCII 大小寫不敏感**（argocd / ArgoCD / ARGOCD 皆匹配；
/// 非 ASCII 字元如中文仍需精確相等）。空表 → 原樣回傳。順序由 [`parse_replacements`] 決定
/// （長規則先套）。
pub fn apply_replacements(text: &str, replacements: &Replacements) -> String {
    let mut out = text.to_string();
    for (from, to) in replacements {
        out = replace_case_insensitive(&out, from, to);
    }
    out
}

/// 對 `text` 做 ASCII 大小寫不敏感的全部取代：以 char 為單位掃描，命中 `from`（ASCII 忽略
/// 大小寫、非 ASCII 精確相等）即輸出 `to` 並跳過該段，否則原樣輸出。`from` 空 → 原樣回傳。
fn replace_case_insensitive(text: &str, from: &str, to: &str) -> String {
    let from_chars: Vec<char> = from.chars().collect();
    if from_chars.is_empty() {
        return text.to_string();
    }
    let text_chars: Vec<char> = text.chars().collect();
    let n = from_chars.len();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text_chars.len() {
        let matches = i + n <= text_chars.len()
            && text_chars[i..i + n]
                .iter()
                .zip(&from_chars)
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if matches {
            out.push_str(to);
            i += n;
        } else {
            out.push(text_chars[i]);
            i += 1;
        }
    }
    out
}

/// 句級滾動校正的 printer 事件（ADR-0006 §2.4 協定；Phase 2）。
///
/// 語意：`Partial` 只代表**當前未定稿句**的草稿；`PhraseFinal` 代表**該句 Whisper 定稿並鎖定**；
/// `Final` 等同最後一句的 `PhraseFinal`（呼叫端另負責寫剪貼簿，整段 = [`PhrasePrinter::committed`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhraseEvent<'a> {
    /// 新錄音 session 開始：清空鎖定前綴與當前草稿（避免上次殘留算出錯誤 backspace）。
    SessionStarted,
    /// 當前句的最新草稿（Apple 即時 partial，已裁掉 committed 前綴）。
    Partial(&'a str),
    /// 當前句 Whisper 定稿 → 對齊後鎖定，之後不再更動。
    PhraseFinal(&'a str),
    /// 最後一句定稿（等同 `PhraseFinal`；剪貼簿由呼叫端處理）。
    Final(&'a str),
    /// 錯誤：與 `SessionStarted` 同樣重置草稿狀態。
    Error,
}

/// 句級滾動校正的 printer 狀態機（ADR-0006 §2.5，純函式核心）。
///
/// 維持兩段狀態：
/// - `committed`：已鎖定（已 Whisper 校正）句子的累積前綴，**永不 backspace 越過它**；
/// - `last_partial`：**當前句**的草稿，printer 的 backspace 只在這段內。
///
/// **關鍵安全不變式**（ADR-0006 §2.5 需求「甲」）：因 [`apply`](Self::apply) 回傳的
/// `StreamDiff.backspace` 上限為 `last_partial` 的字元數，游標永遠不會退回 `committed`
/// → **不會刪到使用者已在鎖定句上手動修改的內容**。此性質由 `apply` 的建構方式保證，
/// 並有測試 `backspace_never_exceeds_last_partial` 鎖住。
#[derive(Debug, Default, Clone)]
pub struct PhrasePrinter {
    committed: String,
    last_partial: String,
}

impl PhrasePrinter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已鎖定的累積前綴（Final 時呼叫端用來寫剪貼簿）。
    pub fn committed(&self) -> &str {
        &self.committed
    }

    /// 當前句尚未鎖定的草稿。
    pub fn last_partial(&self) -> &str {
        &self.last_partial
    }

    /// 套用一個事件，回傳呼叫端應執行的注入動作（先 backspace 再 append）。
    ///
    /// 回傳的 `StreamDiff.backspace` 恆 ≤ `last_partial` 字元數（見型別 doc 的不變式）。
    pub fn apply(&mut self, event: PhraseEvent<'_>) -> StreamDiff {
        match event {
            PhraseEvent::SessionStarted | PhraseEvent::Error => {
                // 重置草稿與鎖定前綴；不對已鎖定內容 backspace（新 session 從空白輸入起算）。
                self.committed.clear();
                self.last_partial.clear();
                StreamDiff {
                    backspace: 0,
                    append: String::new(),
                }
            }
            PhraseEvent::Partial(text) => {
                let diff = compute_stream_diff(&self.last_partial, text);
                self.last_partial = text.to_string();
                diff
            }
            PhraseEvent::PhraseFinal(text) | PhraseEvent::Final(text) => {
                // 對齊當前草稿 → 定稿文字，然後鎖定：committed 追加、草稿清空。
                let diff = compute_stream_diff(&self.last_partial, text);
                self.committed.push_str(text);
                self.last_partial.clear();
                diff
            }
        }
    }
}

/// 文字注入介面。測試以 `FakeBackend` 替代，生產環境使用 [`EnigoBackend`]。
pub trait InputBackend {
    /// 鍵入文字到目前 focus 的輸入框。
    fn inject(&mut self, text: &str) -> Result<(), RaflowError>;

    /// 模擬 N 次 Backspace；`count == 0` 必須是 no-op。
    fn backspace(&mut self, count: usize) -> Result<(), RaflowError>;

    /// 把輸入框中既有的 `prev` 替換為 `new`：先 backspace 共同 prefix 之後的字數，
    /// 再鍵入差異 suffix。任一步失敗即提早回傳，不嘗試 rollback。
    fn stream_update(&mut self, prev: &str, new: &str) -> Result<(), RaflowError> {
        let diff = compute_stream_diff(prev, new);
        if diff.backspace > 0 {
            self.backspace(diff.backspace)?;
        }
        if !diff.append.is_empty() {
            self.inject(&diff.append)?;
        }
        Ok(())
    }
}

/// 剪貼簿寫入介面。生產環境使用 [`ArboardClipboard`]。
pub trait ClipboardBackend {
    fn copy(&mut self, text: &str) -> Result<(), RaflowError>;
}

#[cfg(target_os = "macos")]
pub use mac::{ArboardClipboard, EnigoBackend};

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use arboard::Clipboard;
    use enigo::{Direction, Enigo, Key, Keyboard, Settings};

    /// 生產環境的文字注入後端：薄層包裝 [`enigo::Enigo`]。
    ///
    /// 在 macOS 透過 `CGEventPost` 模擬鍵盤事件，需使用者於系統設定
    /// 「隱私權與安全性 → 輔助使用」中對 raflow 執行程序授權。
    pub struct EnigoBackend {
        enigo: Enigo,
    }

    impl EnigoBackend {
        pub fn new() -> Result<Self, RaflowError> {
            let enigo = Enigo::new(&Settings::default()).map_err(|e| RaflowError::TextInject {
                detail: format!("failed to init enigo: {e}"),
            })?;
            Ok(Self { enigo })
        }
    }

    impl InputBackend for EnigoBackend {
        fn inject(&mut self, text: &str) -> Result<(), RaflowError> {
            if text.is_empty() {
                return Ok(());
            }
            self.enigo.text(text).map_err(|e| RaflowError::TextInject {
                detail: format!("failed to type text: {e}"),
            })
        }

        fn backspace(&mut self, count: usize) -> Result<(), RaflowError> {
            for _ in 0..count {
                self.enigo
                    .key(Key::Backspace, Direction::Click)
                    .map_err(|e| RaflowError::TextInject {
                        detail: format!("failed to send backspace: {e}"),
                    })?;
            }
            Ok(())
        }
    }

    /// 生產環境的剪貼簿後端：薄層包裝 [`arboard::Clipboard`]（NSPasteboard）。
    pub struct ArboardClipboard {
        clipboard: Clipboard,
    }

    impl ArboardClipboard {
        pub fn new() -> Result<Self, RaflowError> {
            let clipboard = Clipboard::new().map_err(|e| RaflowError::ClipboardWrite {
                detail: format!("failed to init clipboard: {e}"),
            })?;
            Ok(Self { clipboard })
        }
    }

    impl ClipboardBackend for ArboardClipboard {
        fn copy(&mut self, text: &str) -> Result<(), RaflowError> {
            if text.is_empty() {
                return Ok(());
            }
            self.clipboard
                .set_text(text)
                .map_err(|e| RaflowError::ClipboardWrite {
                    detail: format!("failed to set clipboard text: {e}"),
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FakeInput 紀錄 inject / backspace 的呼叫**順序**（以 `Op` 統一收集），
    /// 用於驗證 `stream_update` 是否依規格先 backspace 再 inject。
    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        Type(String),
        Backspace(usize),
    }

    struct FakeInput {
        ops: Vec<Op>,
        error_on_inject: Option<String>,
        error_on_backspace: bool,
    }

    impl FakeInput {
        fn new() -> Self {
            Self {
                ops: Vec::new(),
                error_on_inject: None,
                error_on_backspace: false,
            }
        }

        fn with_error_on_inject(trigger: &str) -> Self {
            Self {
                ops: Vec::new(),
                error_on_inject: Some(trigger.to_string()),
                error_on_backspace: false,
            }
        }

        fn with_error_on_backspace() -> Self {
            Self {
                ops: Vec::new(),
                error_on_inject: None,
                error_on_backspace: true,
            }
        }

        fn typed(&self) -> Vec<String> {
            self.ops
                .iter()
                .filter_map(|op| match op {
                    Op::Type(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    impl InputBackend for FakeInput {
        fn inject(&mut self, text: &str) -> Result<(), RaflowError> {
            if text.is_empty() {
                return Ok(());
            }
            if let Some(trigger) = &self.error_on_inject {
                if trigger == text {
                    return Err(RaflowError::TextInject {
                        detail: format!("fake failure on {text:?}"),
                    });
                }
            }
            self.ops.push(Op::Type(text.to_string()));
            Ok(())
        }

        fn backspace(&mut self, count: usize) -> Result<(), RaflowError> {
            if count == 0 {
                return Ok(());
            }
            if self.error_on_backspace {
                return Err(RaflowError::TextInject {
                    detail: format!("fake backspace failure (count={count})"),
                });
            }
            self.ops.push(Op::Backspace(count));
            Ok(())
        }
    }

    struct FakeClipboard {
        copied: Vec<String>,
        fail_next: bool,
    }

    impl FakeClipboard {
        fn new() -> Self {
            Self {
                copied: Vec::new(),
                fail_next: false,
            }
        }
    }

    impl ClipboardBackend for FakeClipboard {
        fn copy(&mut self, text: &str) -> Result<(), RaflowError> {
            if text.is_empty() {
                return Ok(());
            }
            if self.fail_next {
                self.fail_next = false;
                return Err(RaflowError::ClipboardWrite {
                    detail: "fake clipboard failure".into(),
                });
            }
            self.copied.push(text.to_string());
            Ok(())
        }
    }

    #[test]
    fn non_empty_text_is_recorded() {
        let mut fake = FakeInput::new();
        let cases = ["你好", "hello world", "數字 123", "end.\n"];
        for text in cases {
            assert!(fake.inject(text).is_ok(), "inject({text:?})");
        }
        assert_eq!(
            fake.typed(),
            vec![
                "你好".to_string(),
                "hello world".to_string(),
                "數字 123".to_string(),
                "end.\n".to_string(),
            ],
        );
    }

    #[test]
    fn empty_text_is_noop() {
        let mut fake = FakeInput::new();
        assert!(fake.inject("").is_ok());
        assert!(fake.ops.is_empty(), "empty inject must not record a call");
    }

    #[test]
    fn input_backend_surfaces_injection_errors() {
        let mut fake = FakeInput::with_error_on_inject("boom");
        let err = fake.inject("boom").unwrap_err();
        assert!(matches!(err, RaflowError::TextInject { .. }));
        assert!(fake.ops.is_empty());
        assert!(fake.inject("ok").is_ok());
        assert_eq!(fake.typed(), vec!["ok".to_string()]);
    }

    #[test]
    fn clipboard_records_non_empty_text() {
        let mut fake = FakeClipboard::new();
        assert!(fake.copy("你好").is_ok());
        assert!(fake.copy("").is_ok(), "empty string must be no-op");
        assert_eq!(fake.copied, vec!["你好".to_string()]);
    }

    #[test]
    fn clipboard_surfaces_write_errors() {
        let mut fake = FakeClipboard::new();
        fake.fail_next = true;
        let err = fake.copy("boom").unwrap_err();
        assert!(matches!(err, RaflowError::ClipboardWrite { .. }));
        assert!(fake.copied.is_empty());
        assert!(fake.copy("ok").is_ok());
        assert_eq!(fake.copied, vec!["ok".to_string()]);
    }

    /// 對齊 spec/input.md §4 邊界條件表。
    #[test]
    fn compute_stream_diff_matches_spec_table() {
        let cases = [
            // (label, prev, new, expected_backspace, expected_append)
            ("fresh start", "", "我", 0, "我"),
            ("append one", "我", "我的", 0, "的"),
            ("append two", "我的", "我的筆", 0, "筆"),
            ("append after final-ish", "我的筆", "我的筆電", 0, "電"),
            ("shrink", "師姐姐", "師姐", 1, ""),
            ("rewrite", "你好", "我好", 2, "我好"),
            ("full replace", "abc", "xyz", 3, "xyz"),
            ("clear", "abc", "", 3, ""),
            ("noop", "abc", "abc", 0, ""),
            ("empty pair", "", "", 0, ""),
            ("ascii append", "hello", "hello!", 0, "!"),
        ];
        for (label, prev, new, backspace, append) in cases {
            let diff = compute_stream_diff(prev, new);
            assert_eq!(
                diff,
                StreamDiff {
                    backspace,
                    append: append.to_string()
                },
                "case {label:?}: prev={prev:?} new={new:?}",
            );
        }
    }

    #[test]
    fn stream_update_typical_partial_sequence() {
        // 模擬 Apple Speech 一連串 partial：「我 → 我的 → 我的筆 → 我的筆電」
        let mut fake = FakeInput::new();
        let stream = ["", "我", "我的", "我的筆", "我的筆電"];
        for window in stream.windows(2) {
            assert!(fake.stream_update(window[0], window[1]).is_ok());
        }
        // 全程都是 append-only，不該有任何 backspace。
        assert_eq!(
            fake.ops,
            vec![
                Op::Type("我".into()),
                Op::Type("的".into()),
                Op::Type("筆".into()),
                Op::Type("電".into()),
            ],
        );
    }

    #[test]
    fn stream_update_handles_rewrite() {
        // partial 1: 師姐姐 → partial 2: 師姐（Apple Speech 自我修正）
        let mut fake = FakeInput::new();
        assert!(fake.stream_update("", "師姐姐").is_ok());
        assert!(fake.stream_update("師姐姐", "師姐").is_ok());
        assert_eq!(fake.ops, vec![Op::Type("師姐姐".into()), Op::Backspace(1)],);
    }

    #[test]
    fn stream_update_handles_full_replace() {
        // partial 大幅 rewrite：你好 → 我好。先 backspace 2 個 char，再 type "我好"。
        let mut fake = FakeInput::new();
        assert!(fake.stream_update("", "你好").is_ok());
        assert!(fake.stream_update("你好", "我好").is_ok());
        assert_eq!(
            fake.ops,
            vec![
                Op::Type("你好".into()),
                Op::Backspace(2),
                Op::Type("我好".into()),
            ],
        );
    }

    #[test]
    fn stream_update_propagates_backspace_error() {
        let mut fake = FakeInput::with_error_on_backspace();
        let err = fake.stream_update("abc", "ab").unwrap_err();
        assert!(matches!(err, RaflowError::TextInject { .. }));
        assert!(fake.ops.is_empty(), "backspace error must short-circuit");
    }

    #[test]
    fn stream_update_propagates_inject_error() {
        // 共同 prefix=0 → backspace=3 + append="xyz"；inject("xyz") 觸發錯誤。
        let mut fake = FakeInput::with_error_on_inject("xyz");
        let err = fake.stream_update("abc", "xyz").unwrap_err();
        assert!(matches!(err, RaflowError::TextInject { .. }));
        // backspace 已執行，但 inject 失敗
        assert_eq!(fake.ops, vec![Op::Backspace(3)]);
    }

    #[test]
    fn stream_update_noop_on_identical_text() {
        let mut fake = FakeInput::new();
        assert!(fake.stream_update("abc", "abc").is_ok());
        assert!(fake.ops.is_empty());
    }

    // ===== PhrasePrinter（ADR-0006 §2.5 句級滾動 printer 純核心）=====

    /// 典型滾動序列：句 1 逐步 partial → PhraseFinal 鎖定 → 句 2 partial → Final。
    /// 驗證每步的注入動作與 committed/last_partial 狀態。
    #[test]
    fn phrase_printer_rolling_sequence() {
        let mut p = PhrasePrinter::new();

        // 句 1 草稿逐步成長：backspace=0，純 append 增量。
        assert_eq!(p.apply(PhraseEvent::Partial("你好")), diff(0, "你好"));
        assert_eq!(p.apply(PhraseEvent::Partial("你好世界")), diff(0, "世界"));

        // 句 1 Whisper 定稿（改了一個字）：對齊當前草稿 → 鎖定。
        // "你好世界" → "你好，世界" 共同前綴 "你好" → backspace 2 + append "，世界"。
        assert_eq!(
            p.apply(PhraseEvent::PhraseFinal("你好，世界")),
            diff(2, "，世界")
        );
        assert_eq!(p.committed(), "你好，世界");
        assert_eq!(p.last_partial(), "");

        // 句 2 從空草稿起算：純 append，不碰 committed。
        assert_eq!(p.apply(PhraseEvent::Partial("再見")), diff(0, "再見"));
        // 句 2 Final：草稿 "再見" 與定稿 "再見。" 共同前綴 "再見" → backspace 0，純 append "。"。
        assert_eq!(p.apply(PhraseEvent::Final("再見。")), diff(0, "。"));
        assert_eq!(p.committed(), "你好，世界再見。");
        assert_eq!(p.last_partial(), "");
    }

    /// 關鍵安全不變式：任何事件回傳的 backspace 都 ≤ 當前 last_partial 長度，
    /// 游標永不退回 committed（不會刪到已鎖定/使用者手改的舊句）。
    #[test]
    fn phrase_printer_backspace_never_exceeds_last_partial() {
        // 先鎖定一段長 committed，再灌一個「完全不同」的短草稿與定稿，
        // 確認 backspace 只吃當前草稿，不越界。
        let cases: &[(&str, &str)] = &[
            ("當前草稿很長很長很長", "短"), // partial 很長 → PhraseFinal 短
            ("abc", "完全不同"),
            ("", "從空草稿定稿"),
            ("重疊前綴xyz", "重疊前綴123"),
        ];
        for (draft, finalized) in cases {
            let mut p = PhrasePrinter::new();
            p.apply(PhraseEvent::PhraseFinal("已鎖定的第一句很長")); // committed 前綴
            let before_len = draft.chars().count();
            p.apply(PhraseEvent::Partial(draft));
            let d = p.apply(PhraseEvent::PhraseFinal(finalized));
            assert!(
                d.backspace <= before_len,
                "backspace {} 超過當前草稿長度 {} (draft={draft:?})",
                d.backspace,
                before_len
            );
        }
    }

    /// SessionStarted 與 Error 都重置草稿與 committed，回傳 no-op（不對已輸入內容 backspace）。
    #[test]
    fn phrase_printer_reset_events_are_noop_and_clear_state() {
        for reset in [PhraseEvent::SessionStarted, PhraseEvent::Error] {
            let mut p = PhrasePrinter::new();
            p.apply(PhraseEvent::Partial("草稿"));
            p.apply(PhraseEvent::PhraseFinal("鎖定句"));
            let d = p.apply(reset);
            assert_eq!(d, diff(0, ""), "reset 應為 no-op");
            assert_eq!(p.committed(), "");
            assert_eq!(p.last_partial(), "");
        }
    }

    fn diff(backspace: usize, append: &str) -> StreamDiff {
        StreamDiff {
            backspace,
            append: append.to_string(),
        }
    }

    // ===== 取代對照表（使用者自訂術語硬性修正）=====

    #[test]
    fn parse_replacements_skips_junk_and_sorts_by_length_desc() {
        let contents = "\
# 這是註解\n\
狗 => dog\n\
阿狗CD => ArgoCD\n\
\n\
  卡夫卡  =>  Kafka  \n\
# 另一個註解\n\
沒有箭頭的行\n\
=> 缺左邊\n\
缺右邊 =>\n\
K8S => Kubernetes\n";
        let reps = parse_replacements(contents);
        // 只保留合法規則；且**依 from 長度遞減排序**（4>3>1），等長者維持檔案順序（卡夫卡 先於 K8S）。
        assert_eq!(
            reps,
            vec![
                ("阿狗CD".to_string(), "ArgoCD".to_string()),  // 4 chars
                ("卡夫卡".to_string(), "Kafka".to_string()),   // 3（trim）
                ("K8S".to_string(), "Kubernetes".to_string()), // 3
                ("狗".to_string(), "dog".to_string()),         // 1（雖在檔案最前，排到最後）
            ],
        );
    }

    #[test]
    fn apply_replacements_is_case_insensitive_and_longest_first() {
        // parse 已排序：長的「阿狗CD」先於短的「狗」→ 短規則不會先把字拆掉。
        let reps = parse_replacements("狗 => dog\n阿狗CD => ArgoCD");
        assert_eq!(
            apply_replacements("我們的阿狗CD工具", &reps),
            "我們的ArgoCD工具"
        );

        // ASCII 大小寫不敏感：規則 argocd 可匹配 ARGOCD / ArgoCD。
        let ci = parse_replacements("argocd => ArgoCD");
        assert_eq!(
            apply_replacements("我用 ARGOCD 部署", &ci),
            "我用 ArgoCD 部署"
        );
        assert_eq!(apply_replacements("跑 argocd sync", &ci), "跑 ArgoCD sync");
        assert_eq!(apply_replacements("ArgoCD 已存在", &ci), "ArgoCD 已存在");

        // 無匹配 → 原樣；空表 → 原樣（無副作用）。
        assert_eq!(apply_replacements("純文字", &ci), "純文字");
        assert_eq!(apply_replacements("阿狗CD", &Vec::new()), "阿狗CD");
    }
}
