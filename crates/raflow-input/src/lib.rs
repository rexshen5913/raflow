//! raflow-input：把 partial / final transcript 注入到使用者目前 focus 的輸入框，
//! 並於 Final 時同時複製到系統剪貼簿作 fallback。
//!
//! 完整範圍見 `docs/spec/input.md`。

use raflow_core::RaflowError;
use std::collections::VecDeque;
use std::path::Path;

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
    rules.sort_by_key(|(from, _)| std::cmp::Reverse(from.chars().count()));
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

/// 把一組 `(heard, correct)` 更正**加入或更新**到 replacements 檔內容，回傳新內容（純函式、
/// 無 I/O；D1 更正回饋迴圈，見 `docs/design/vocabulary-growth.md`）。
///
/// - 兩側 trim；任一側空 → `Ok(原樣)`（no-op，該詞無可修正）。
/// - `heard` 與 `correct`（trim 後）精確相等 → `Ok(原樣)`（no-op，不污染詞庫）。大小寫不同
///   （`argocd → ArgoCD`）仍是有效正規化修正，不在此擋。
/// - **輸入驗證（UI 貼上／輸入的邊界防線）**：任一側含換行（`\n`／`\r`），或 `heard` 含分隔符
///   `=>`、或 `heard`（trim 後）以 `#` 開頭 → `Err(InvalidReplacement)`。此保證每次成功 upsert 都恰好
///   產生**一條**可被 [`parse_replacements`] 解回、且 `from == heard.trim()`、`to == correct.trim()`
///   的規則，不會被行導向元字元破壞或注入額外規則。（`correct` 含 `=>` 因 `split_once` 只切首個
///   分隔符仍能 round-trip，故允許。）
/// - 命中既有同 `heard` 規則（非註解行、`from` 段 ASCII 大小寫不敏感相等）→ **就地整行**換成
///   `heard => correct`，保留其餘行與註解、行序；否則 **append** 於檔尾。
/// - 一律確保結尾單一換行。長度排序由 [`parse_replacements`] 於載入時負責，故此處不排序。
pub fn upsert_replacement(
    existing: &str,
    heard: &str,
    correct: &str,
) -> Result<String, RaflowError> {
    let heard = heard.trim();
    let correct = correct.trim();
    if heard.is_empty() || correct.is_empty() {
        return Ok(existing.to_string());
    }
    // 精確相等 → 該詞本就正確、無可修正，no-op（不污染詞庫）。大小寫不同（argocd → ArgoCD）
    // 仍是有效的正規化修正，不在此擋。
    if heard == correct {
        return Ok(existing.to_string());
    }
    // 行導向元字元防線：換行會裂成／注入額外規則；`heard` 含 `=>` 或起首 `#` 則寫回後無法以相同
    // 語意被 parse_replacements 解回（前者切錯分隔、後者被當註解）。
    let has_linebreak = |s: &str| s.contains('\n') || s.contains('\r');
    if has_linebreak(heard) || has_linebreak(correct) {
        return Err(RaflowError::InvalidReplacement {
            detail: "heard/correct 不可含換行".to_string(),
        });
    }
    if heard.contains("=>") {
        return Err(RaflowError::InvalidReplacement {
            detail: "heard 不可含分隔符 =>".to_string(),
        });
    }
    if heard.starts_with('#') {
        return Err(RaflowError::InvalidReplacement {
            detail: "heard 不可以 # 開頭（會被當作註解）".to_string(),
        });
    }
    let new_rule = format!("{heard} => {correct}");

    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        let t = line.trim();
        if !replaced && !t.is_empty() && !t.starts_with('#') {
            if let Some((from, _)) = t.split_once("=>") {
                if from.trim().eq_ignore_ascii_case(heard) {
                    out.push(new_rule.clone());
                    replaced = true;
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }
    if !replaced {
        out.push(new_rule);
    }
    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// 原子寫入文字檔（D1 更正回饋迴圈，見 `docs/design/vocabulary-growth.md` §5／§6）：於目標
/// **同目錄**建立**唯一命名**（`NamedTempFile`，O_EXCL）的暫存檔，寫入後 `persist`（原子 rename、
/// 同檔案系統）覆蓋目標，避免寫到一半崩潰留下半截 `replacements.txt`。必要時建立父目錄。
///
/// 用唯一命名而非固定 `.tmp`：後者會**撞掉並刪除**使用者剛好同名的既有檔、且並行寫入互毀
/// （Codex review）。中途失敗時暫存檔於 drop 自動清除。任何 I/O 失敗 → `Err(ConfigWrite)`。
pub fn atomic_write(path: &Path, contents: &str) -> Result<(), RaflowError> {
    use std::io::Write as _;
    let write_err = |p: &Path| {
        let p = p.to_path_buf();
        move |source| RaflowError::ConfigWrite { path: p, source }
    };
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(write_err(parent))?;
    }
    // 暫存檔須與目標同檔案系統（rename 才原子），故建在目標父目錄；無父目錄則用當前目錄。
    let dir = parent.unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(write_err(dir))?;
    tmp.write_all(contents.as_bytes())
        .map_err(write_err(tmp.path()))?;
    tmp.persist(path).map_err(|e| RaflowError::ConfigWrite {
        path: path.to_path_buf(),
        source: e.error,
    })?;
    Ok(())
}

/// 讀取 `path`（不存在 → 視為空）→ [`upsert_replacement`] → 內容有變才 [`atomic_write`] 寫回
/// （D1 更正回饋迴圈，見 `docs/design/vocabulary-growth.md` §5／§6）。驗證錯誤在**任何寫入前**
/// 就傳回（`upsert_replacement` 先跑），故非法輸入不會動到檔案；no-op（空／精確相等／無變化）
/// 也不觸碰檔案。讀失敗（非「不存在」）→ `Err(ConfigLoad)`。
pub fn upsert_replacement_file(path: &Path, heard: &str, correct: &str) -> Result<(), RaflowError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(RaflowError::ConfigLoad {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let updated = upsert_replacement(&existing, heard, correct)?;
    if updated != existing {
        atomic_write(path, &updated)?;
    }
    Ok(())
}

/// 把一個術語加進／提升到 `contextual_terms.txt` 的**優先區頂端**，回傳新內容（純函式、無 I/O；
/// D1 更正回饋迴圈「也加優先區」，見 `docs/design/vocabulary-growth.md` §3／§5）。
///
/// - term trim；空 → `Ok(原樣)`（no-op）。
/// - 含換行（`\n`／`\r`）或起首 `#` → `Err(InvalidReplacement)`（會裂成多個術語或被當註解）。
/// - **已在優先區頂端**（第一個術語行 trim 後即為此 term）→ `Ok(原樣)`（no-op，保留原檔）。
/// - **已存在但埋在下方** → **提升**：移除所有既有出現（非註解行、trim 後相等），再把 normalized
///   `term` 插到第一個剩餘術語行之前。這是「也加優先區」的產品語意——否則第 30 名以後的舊術語勾了
///   也進不了 priming（Codex review）。
/// - 不存在 → 插到第一個術語行之前；無術語行則置於所有行之後。
///
/// 優先區頂端 = 最高 priming 優先；實際 prompt 上限由 `ROLLING_PROMPT_TERM_CAP`
/// （2026-07-07 由 20 調為 30）經 `raflow-speech::build_rolling_prompt(...).take(...)` 施加，超出者仍保存只是
/// 不進 priming。保留既有註解與空行順序，結尾單一換行。與 `merge_contextual_terms` 同格式。
pub fn upsert_contextual_priority_term(existing: &str, term: &str) -> Result<String, RaflowError> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(existing.to_string());
    }
    if term.contains('\n') || term.contains('\r') {
        return Err(RaflowError::InvalidReplacement {
            detail: "術語不可含換行".to_string(),
        });
    }
    if term.starts_with('#') {
        return Err(RaflowError::InvalidReplacement {
            detail: "術語不可以 # 開頭（會被當作註解）".to_string(),
        });
    }
    let is_term = |l: &str| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with('#')
    };
    // 已在優先區頂端（第一個術語行就是此 term）→ no-op，原檔照留（保留使用者原本寫法）。
    if existing
        .lines()
        .find(|l| is_term(l))
        .is_some_and(|first| first.trim() == term)
    {
        return Ok(existing.to_string());
    }
    // 否則重建：丟掉所有既有出現，把 normalized term 插到第一個剩餘術語行之前（提升到頂端）。
    let mut out: Vec<String> = Vec::new();
    let mut inserted = false;
    for line in existing.lines() {
        if is_term(line) && line.trim() == term {
            continue; // 移除舊位置的出現，稍後於頂端重插
        }
        if is_term(line) && !inserted {
            out.push(term.to_string());
            inserted = true;
        }
        out.push(line.to_string());
    }
    if !inserted {
        out.push(term.to_string());
    }
    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// 讀取 `path`（不存在 → 視為空）→ [`upsert_contextual_priority_term`] → 內容有變才 [`atomic_write`]
/// 寫回（D1「也加優先區」的檔案版，見 `docs/design/vocabulary-growth.md` §5）。
///
/// 與 [`upsert_replacement_file`] 同策略——**讀失敗（非「不存在」）→ `Err(ConfigLoad)`，不當空檔**：
/// 否則暫時不可讀／權限異常／非 UTF-8 時會把整個既有詞庫覆蓋成單一術語（Codex round-2 資料遺失）。
/// 驗證錯誤在任何寫入前傳回；no-op（空／已在頂端／無變化）不觸碰檔案。
pub fn upsert_contextual_priority_term_file(path: &Path, term: &str) -> Result<(), RaflowError> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(RaflowError::ConfigLoad {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let updated = upsert_contextual_priority_term(&existing, term)?;
    if updated != existing {
        atomic_write(path, &updated)?;
    }
    Ok(())
}

/// 從一句已注入文字抽出「英文 token」候選（D1 更正回饋迴圈，見
/// `docs/design/vocabulary-growth.md` §6／§10 Phase 2）：供擷取 UI 的『聽成』下拉快速挑選。
///
/// 語意：切出最大的 **ASCII 英數 run**（`is_ascii_alphanumeric`），非 ASCII（中文）、標點、
/// 空白皆為分隔；僅保留**至少含一個 ASCII 字母**的 run（純數字如 `123` 排除）。依原序回傳，
/// **不去重、不設上限**——去重與上限屬「最近注入緩衝」職責（§11 待決），此處只負責單句抽取。
pub fn extract_english_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            if current.chars().any(|c| c.is_ascii_alphabetic()) {
                tokens.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if !current.is_empty() && current.chars().any(|c| c.is_ascii_alphabetic()) {
        tokens.push(current);
    }
    tokens
}

/// 本 session「最近注入的英文 token」候選緩衝（D1 更正回饋迴圈，見
/// `docs/design/vocabulary-growth.md` §6／§9／§11；使用者定案「最近 N 句去重、保留最新」）。
///
/// 保留最近 `cap` **句**的 [`extract_english_tokens`] 結果；[`candidates`](Self::candidates) 以
/// **最新句在前**、跨句去重（同 token 只留最新出現一次）回傳，供擷取 UI 的『聽成』下拉。
/// 純資料結構、有上限、隨 session 丟棄（§9 隱私：僅記憶體、不外送）。無英文的句不佔額度。
#[derive(Debug, Clone)]
pub struct RecentTokens {
    cap: usize,
    /// 每格為一句抽出的 token（front = 最舊、back = 最新）。
    sentences: VecDeque<Vec<String>>,
}

impl RecentTokens {
    /// 以「保留最近 `cap` 句」建構；`cap == 0` → 永遠不留任何候選。
    pub fn new(cap: usize) -> Self {
        Self {
            cap,
            sentences: VecDeque::new(),
        }
    }

    /// 推入一句已注入文字：抽其英文 token 存為一句。無 token（純中文）→ 不佔額度、不擠掉既有；
    /// 超過 `cap` 句則丟最舊。
    pub fn push_sentence(&mut self, text: &str) {
        if self.cap == 0 {
            return;
        }
        let tokens = extract_english_tokens(text);
        if tokens.is_empty() {
            return;
        }
        self.sentences.push_back(tokens);
        while self.sentences.len() > self.cap {
            self.sentences.pop_front();
        }
    }

    /// 候選清單：最新句在前、句內維持出現順序，跨句去重（保留最新那一次出現）。
    pub fn candidates(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for sentence in self.sentences.iter().rev() {
            for tok in sentence {
                if seen.insert(tok.as_str()) {
                    out.push(tok.clone());
                }
            }
        }
        out
    }
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

/// 注入焦點守衛（security audit run-1 Finding 1 修復；純狀態機）。
///
/// 問題：printer 的注入目標由「投遞當下的系統焦點」決定，但焦點只在 session 起點
/// 偵測一次。使用者錄音中 Cmd+Tab 切走後，後續 partial / final 的文字與 backspace
/// 會打進**新聚焦的 app**，backspace 數量還是按舊輸入框草稿算的 → 刪到別人的內容。
///
/// 守衛語意：
/// - `session_started(pid)`：記錄 session 起點的前景 app PID 作為基準，解除上輪閂鎖；
/// - `should_inject(current)`：PID 相同 → 允許；PID 改變 → 拒絕並**閂住整個 session**
///   （焦點切回原 app 也不恢復——閂住期間錯過的 diff 使 backspace 與輸入框內容對不上，
///   再注入只會毀損文字；Final 的剪貼簿 fallback 不受影響，使用者可 Cmd+V 取回全文）；
/// - 任一側 PID 為 `None`（AX 未授權 / 查詢失敗）→ **fail-open** 維持既有注入行為，
///   避免權限不足時整個聽寫失效（此時 enigo 注入本來就走獨立的權限路徑）。
#[derive(Debug, Default, Clone)]
pub struct FocusGuard {
    session_pid: Option<i32>,
    latched_off: bool,
}

impl FocusGuard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 新 session 起點：記錄前景 app PID 基準並解除閂鎖。
    pub fn session_started(&mut self, pid: Option<i32>) {
        self.session_pid = pid;
        self.latched_off = false;
    }

    /// 是否已因焦點切換而閂住本 session 的注入。呼叫端用來做「每 session 只提示一次」
    /// 的 stderr 警告（閂鎖在 `should_inject` 內由 false → true 的那一次即為提示時機）。
    pub fn latched(&self) -> bool {
        self.latched_off
    }

    /// 每次注入前呼叫：回傳是否允許注入。語意見型別 doc。
    pub fn should_inject(&mut self, current_pid: Option<i32>) -> bool {
        if self.latched_off {
            return false;
        }
        match (self.session_pid, current_pid) {
            (Some(session), Some(current)) if session != current => {
                self.latched_off = true;
                false
            }
            _ => true,
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

    /// D1 更正回饋迴圈（docs/design/vocabulary-growth.md §8）：把 (heard, correct) 更正
    /// 加入／更新到 replacements 檔內容，保留既有註解與行序，結尾單一換行。
    #[test]
    fn upsert_replacement_adds_updates_and_preserves_file() {
        // 成功案例以 `.ok().as_deref()` 比對內容（避免 unwrap，憲法 §3.1）。
        // 空檔 → append，結尾補換行
        assert_eq!(
            upsert_replacement("", "ANSIPO", "Ansible").ok().as_deref(),
            Some("ANSIPO => Ansible\n")
        );

        // 無尾換行檔 → append 前補換行（不黏在一起）
        assert_eq!(
            upsert_replacement("# 標題\n狗 => dog", "ANSIPO", "Ansible")
                .ok()
                .as_deref(),
            Some("# 標題\n狗 => dog\nANSIPO => Ansible\n")
        );

        // heard 已存在（精確）→ 更新 correct，保留其他行與註解、行序
        assert_eq!(
            upsert_replacement("# c\nANSIPO => Foo\n狗 => dog\n", "ANSIPO", "Ansible")
                .ok()
                .as_deref(),
            Some("# c\nANSIPO => Ansible\n狗 => dog\n")
        );

        // heard 已存在（ASCII 大小寫不同）→ 命中同一行、整行換成新 heard => correct
        assert_eq!(
            upsert_replacement("ansipo => Foo\n", "ANSIPO", "Ansible")
                .ok()
                .as_deref(),
            Some("ANSIPO => Ansible\n")
        );

        // 兩側 trim
        assert_eq!(
            upsert_replacement("", "  ANSIPO  ", "  Ansible  ")
                .ok()
                .as_deref(),
            Some("ANSIPO => Ansible\n")
        );

        // 註解行含「=>」不被當規則 → append 而非誤更新
        assert_eq!(
            upsert_replacement("# a => b\n", "ANSIPO", "Ansible")
                .ok()
                .as_deref(),
            Some("# a => b\nANSIPO => Ansible\n")
        );

        // heard 或 correct 空（trim 後）→ no-op 原樣回傳
        assert_eq!(
            upsert_replacement("狗 => dog\n", "", "X").ok().as_deref(),
            Some("狗 => dog\n")
        );
        assert_eq!(
            upsert_replacement("狗 => dog\n", "X", "   ")
                .ok()
                .as_deref(),
            Some("狗 => dog\n")
        );

        // heard 與 correct（trim 後）精確相等 → no-op（該詞本就正確、無可修正）
        assert_eq!(
            upsert_replacement("狗 => dog\n", "Ansible", "Ansible")
                .ok()
                .as_deref(),
            Some("狗 => dog\n")
        );
        assert_eq!(
            upsert_replacement("狗 => dog\n", " Ansible ", "Ansible")
                .ok()
                .as_deref(),
            Some("狗 => dog\n")
        );

        // 但大小寫不同仍算有效修正（正規化語意）→ 照常記錄，非 no-op
        assert_eq!(
            upsert_replacement("", "argocd", "ArgoCD").ok().as_deref(),
            Some("argocd => ArgoCD\n")
        );

        // correct 含 `=>`：split_once 只切首個分隔符 → 仍能 round-trip，故允許
        assert_eq!(
            upsert_replacement("", "AtoB", "A => B").ok().as_deref(),
            Some("AtoB => A => B\n")
        );
    }

    /// 行導向元字元防線（Codex review round-1）：UI 貼上／輸入若帶換行、`heard` 帶分隔符或起首
    /// `#`，會裂成／注入額外規則或被當註解 → 必須 `Err`，不得寫進檔案破壞格式。
    #[test]
    fn upsert_replacement_rejects_line_oriented_metachars() {
        for (heard, correct) in [
            ("AN\nSIPO", "Ansible"), // heard 換行
            ("ANSIPO", "Ansi\nble"), // correct 換行
            ("AN\rSIPO", "Ansible"), // heard CR
            ("ANSIPO", "Ansi\rble"), // correct CR
            ("A => B", "Ansible"),   // heard 含分隔符 => → 切錯分隔、無法 round-trip
            ("#note", "Ansible"),    // heard 起首 # → 會被當註解丟棄
        ] {
            let r = upsert_replacement("狗 => dog\n", heard, correct);
            assert!(
                matches!(r, Err(RaflowError::InvalidReplacement { .. })),
                "({heard:?}, {correct:?}) 應被拒為 InvalidReplacement，實得 {r:?}"
            );
        }
    }

    /// 成功 upsert 的不變式（Codex review round-1）：產物恰好多出**一條**可被 parse_replacements
    /// 解回、且 `from == heard.trim()`、`to == correct.trim()` 的規則。
    #[test]
    fn upsert_replacement_result_round_trips_through_parser() {
        let cases = [
            ("", "ANSIPO", "Ansible"),
            ("# c\n狗 => dog\n", "  卡夫卡  ", "  Kafka  "),
            ("", "AtoB", "A => B"), // correct 含 => 也要能解回
        ];
        for (existing, heard, correct) in cases {
            let before = parse_replacements(existing).len();
            let out = upsert_replacement(existing, heard, correct)
                .ok()
                .unwrap_or_default();
            let rules = parse_replacements(&out);
            assert_eq!(
                rules.len(),
                before + 1,
                "({existing:?}, {heard:?}, {correct:?}) 應恰好新增一條規則"
            );
            let hit = rules
                .iter()
                .find(|(from, _)| from == heard.trim())
                .map(|(_, to)| to.as_str());
            assert_eq!(
                hit,
                Some(correct.trim()),
                "round-trip 後 from/to 必須等於 trim 後輸入"
            );
        }
    }

    /// D1 更正回饋迴圈（docs/design/vocabulary-growth.md §5／§6）：讀檔（不存在→空）→
    /// `upsert_replacement` → **原子寫回**（temp + rename）。以真實檔案系統測試（憲法 §2.3），
    /// 涵蓋建檔、append 保留、原子性（無殘留 .tmp）、no-op 不動檔、非法輸入不動檔。
    #[test]
    fn upsert_replacement_file_creates_appends_and_stays_atomic()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("replacements.txt");

        // 檔不存在 → 建檔並寫入規則
        upsert_replacement_file(&path, "ANSIPO", "Ansible")?;
        assert_eq!(std::fs::read_to_string(&path)?, "ANSIPO => Ansible\n");

        // 既有檔 → append，保留原行
        upsert_replacement_file(&path, "狗", "dog")?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "ANSIPO => Ansible\n狗 => dog\n"
        );

        // 原子性：目錄內只剩目標檔，無殘留暫存檔
        let mut names: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec![std::ffi::OsString::from("replacements.txt")]);

        // no-op（heard 空，trim 後）→ 內容不變
        upsert_replacement_file(&path, "   ", "X")?;
        assert_eq!(
            std::fs::read_to_string(&path)?,
            "ANSIPO => Ansible\n狗 => dog\n"
        );

        // 非法輸入（heard 含換行）→ Err，且檔案不被更動（驗證發生在任何寫入前）
        let before = std::fs::read_to_string(&path)?;
        let err = upsert_replacement_file(&path, "a\nb", "c");
        assert!(
            matches!(err, Err(RaflowError::InvalidReplacement { .. })),
            "非法輸入應回 InvalidReplacement，實得 {err:?}"
        );
        assert_eq!(std::fs::read_to_string(&path)?, before);

        Ok(())
    }

    /// D1 更正回饋迴圈（docs/design/vocabulary-growth.md §3「也加優先區」／§5）：把一個術語加進
    /// `contextual_terms.txt` 的**優先區頂端**（第一個術語行之前 → 最高 priming 優先）。與
    /// `raflow-speech::merge_contextual_terms` 同格式（`#` 註解、空行略過、trim、exact 去重）。
    #[test]
    fn upsert_contextual_priority_term_prepends_and_dedups() {
        let ok = |e: &str, t: &str| upsert_contextual_priority_term(e, t).ok();
        // 空檔 → 該詞
        assert_eq!(ok("", "Ansible").as_deref(), Some("Ansible\n"));
        // 只有註解 → 置於註解之後
        assert_eq!(ok("# c\n", "Ansible").as_deref(), Some("# c\nAnsible\n"));
        // 註解 + 既有術語 → 插在第一個術語之前（優先區頂端）
        assert_eq!(
            ok("# c\nFoo\nBar\n", "Ansible").as_deref(),
            Some("# c\nAnsible\nFoo\nBar\n")
        );
        // 無尾換行 → 插入後補單一尾換行
        assert_eq!(ok("Foo", "Ansible").as_deref(), Some("Ansible\nFoo\n"));
        // 已在優先區頂端（第一個術語行）→ no-op，保留原檔
        assert_eq!(
            ok("Ansible\nFoo\n", "Ansible").as_deref(),
            Some("Ansible\nFoo\n")
        );
        // 已在頂端（檔案行有前後空白）→ no-op（保留原檔寫法）
        assert_eq!(
            ok("  Ansible  \nFoo\n", "Ansible").as_deref(),
            Some("  Ansible  \nFoo\n")
        );
        // 已存在但埋在下方 → 提升到優先區頂端（移除舊位置）
        assert_eq!(
            ok("Foo\nBar\nAnsible\n", "Ansible").as_deref(),
            Some("Ansible\nFoo\nBar\n")
        );
        // 帶註解的提升：術語提到第一個術語行之前，註解與其餘序保留
        assert_eq!(
            ok("# c\nFoo\nBar\nAnsible\n", "Ansible").as_deref(),
            Some("# c\nAnsible\nFoo\nBar\n")
        );
        // 多處重複 → 提升時一併去除舊出現，頂端只留一個
        assert_eq!(
            ok("Foo\nAnsible\nBar\nAnsible\n", "Ansible").as_deref(),
            Some("Ansible\nFoo\nBar\n")
        );
        // 術語 trim
        assert_eq!(ok("", "  Ansible  ").as_deref(), Some("Ansible\n"));
        // 空術語（trim 後）→ no-op
        assert_eq!(ok("Foo\n", "   ").as_deref(), Some("Foo\n"));
        // 含換行 / 起首 # → Err（會裂成多個術語或被當註解）
        assert!(matches!(
            upsert_contextual_priority_term("", "a\nb"),
            Err(RaflowError::InvalidReplacement { .. })
        ));
        assert!(matches!(
            upsert_contextual_priority_term("", "#note"),
            Err(RaflowError::InvalidReplacement { .. })
        ));
    }

    /// D1（Codex round-2）：contextual_terms 優先區的檔案版——讀（不存在→空）→ upsert → 有變才
    /// 原子寫回。**讀失敗（非「不存在」）不得當空檔**（否則會把既有詞庫覆蓋成單一術語）。
    #[test]
    fn upsert_contextual_priority_term_file_creates_and_promotes()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("contextual_terms.txt");

        // 不存在 → 建檔
        upsert_contextual_priority_term_file(&path, "Ansible")?;
        assert_eq!(std::fs::read_to_string(&path)?, "Ansible\n");

        // 既有且埋在下方 → 提升到頂端
        std::fs::write(&path, "# c\nFoo\nBar\nAnsible\n")?;
        upsert_contextual_priority_term_file(&path, "Ansible")?;
        assert_eq!(std::fs::read_to_string(&path)?, "# c\nAnsible\nFoo\nBar\n");

        // 已在頂端 → no-op（內容不變、無殘留暫存檔）
        let before = std::fs::read_to_string(&path)?;
        upsert_contextual_priority_term_file(&path, "Ansible")?;
        assert_eq!(std::fs::read_to_string(&path)?, before);
        let mut names: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("contextual_terms.txt")]
        );

        // 非法輸入（含換行）→ Err，且檔案不被更動
        let before = std::fs::read_to_string(&path)?;
        assert!(matches!(
            upsert_contextual_priority_term_file(&path, "a\nb"),
            Err(RaflowError::InvalidReplacement { .. })
        ));
        assert_eq!(std::fs::read_to_string(&path)?, before);

        Ok(())
    }

    /// Codex review 回歸：原子寫入不得撞掉使用者同目錄的既有檔——尤其不能用固定 `.tmp` 名，
    /// 否則會覆蓋並刪除剛好同名的無關檔。此測試放一個 `replacements.tmp` 哨兵於同目錄，寫入後
    /// 斷言哨兵原封不動、目標內容正確。
    #[test]
    fn atomic_write_preserves_unrelated_sibling_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let target = dir.path().join("replacements.txt");
        // 使用者剛好有一個與「固定 .tmp 命名」同名的無關檔
        let sentinel = dir.path().join("replacements.tmp");
        std::fs::write(&sentinel, "使用者的重要資料，勿刪")?;

        upsert_replacement_file(&target, "ANSIPO", "Ansible")?;

        assert_eq!(std::fs::read_to_string(&target)?, "ANSIPO => Ansible\n");
        assert_eq!(
            std::fs::read_to_string(&sentinel)?,
            "使用者的重要資料，勿刪",
            "同目錄的無關檔不得被原子寫入撞掉"
        );
        Ok(())
    }

    /// D1 更正回饋迴圈（docs/design/vocabulary-growth.md §6「抽英文 token」、§10 Phase 2）：
    /// 從一句已注入文字抽出「英文 token」候選（供擷取 UI 的『聽成』下拉）。語意——最大 ASCII
    /// 英數 run，且至少含一個 ASCII 字母；非 ASCII（中文）、標點、空白皆為分隔。純數字 run 排除。
    /// **不去重、不設上限**（去重與上限屬最近注入緩衝的職責，見 §11，本函式只負責單句抽取）。
    #[test]
    fn extract_english_tokens_pulls_ascii_word_runs() {
        // 純中文 → 無 token
        assert_eq!(
            extract_english_tokens("我想部署到雲端"),
            Vec::<String>::new()
        );

        // 空字串 → 空
        assert_eq!(extract_english_tokens(""), Vec::<String>::new());

        // 中英混講 → 依序抽出英文 run
        assert_eq!(
            extract_english_tokens("我用 Terraform 跟 K8S 部署 Ansible"),
            vec!["Terraform", "K8S", "Ansible"]
        );

        // 標點相鄰 → 標點不算 token 的一部分
        assert_eq!(
            extract_english_tokens("(ArgoCD) 很好用，用 Helm。"),
            vec!["ArgoCD", "Helm"]
        );

        // 含數字但有字母 → 保留；純數字 run → 排除
        assert_eq!(extract_english_tokens("升級到 v3 版本 123 次"), vec!["v3"]);

        // 連字號、點號為分隔（拆成多個 token）
        assert_eq!(
            extract_english_tokens("large-v3-turbo"),
            vec!["large", "v3", "turbo"]
        );

        // 不去重：同一句重複出現照原序全數保留（去重是緩衝的事）
        assert_eq!(extract_english_tokens("K8S 又 K8S"), vec!["K8S", "K8S"]);
    }

    /// D1 更正回饋迴圈（docs/design/vocabulary-growth.md §6／§11；使用者定案「最近 N 句去重、
    /// 保留最新」）：本 session 最近注入英文 token 的候選緩衝——保留最近 `cap` 句抽取結果，
    /// `candidates()` 以**最新句在前**、跨句去重（同 token 只留最新一次）回傳；無英文的句不佔額度。
    #[test]
    fn recent_tokens_caps_and_dedups_newest_first() {
        // 空緩衝 → 無候選
        assert_eq!(RecentTokens::new(3).candidates(), Vec::<String>::new());

        // 單句 → 句內維持出現順序
        let mut b = RecentTokens::new(3);
        b.push_sentence("用 Terraform 跟 K8S");
        assert_eq!(b.candidates(), vec!["Terraform", "K8S"]);

        // 超過 cap 句 → 丟最舊；最新句在前
        let mut b = RecentTokens::new(2);
        b.push_sentence("Apple");
        b.push_sentence("Banana");
        b.push_sentence("Cherry");
        assert_eq!(b.candidates(), vec!["Cherry", "Banana"]);

        // 跨句去重、保留最新出現：Foo 在舊句與新句都有 → 只留最新句那一次
        let mut b = RecentTokens::new(3);
        b.push_sentence("Foo");
        b.push_sentence("Bar Foo");
        assert_eq!(b.candidates(), vec!["Bar", "Foo"]);

        // 無英文的句（純中文）不佔額度，不擠掉既有候選
        let mut b = RecentTokens::new(1);
        b.push_sentence("Alpha");
        b.push_sentence("純中文沒有英文");
        assert_eq!(b.candidates(), vec!["Alpha"]);

        // cap 0 → 恆空
        let mut b = RecentTokens::new(0);
        b.push_sentence("Alpha");
        assert_eq!(b.candidates(), Vec::<String>::new());
    }

    // ===== FocusGuard（注入焦點守衛；security audit run-1 Finding 1）=====

    /// 決策表：每列為一個 session——`session_started(session_pid)` 後依序對
    /// `should_inject(current_pid)` 逐步斷言。核心語意：
    /// - PID 相同 → 允許注入
    /// - PID 改變 → 拒絕，且**閂住**該 session 直到下一次 `session_started`
    ///   （焦點切回原 app 也不再注入：錯過的 diff 使 backspace 對不上，寧可停手）
    /// - 任一側 PID 為 None（AX 未授權 / 查詢失敗）→ fail-open 維持現行為
    #[test]
    fn focus_guard_decision_table() {
        type Step = (Option<i32>, bool); // (current_pid, expect_inject)
        let cases: &[(&str, Option<i32>, &[Step])] = &[
            (
                "same pid keeps injecting",
                Some(100),
                &[(Some(100), true), (Some(100), true)],
            ),
            (
                "pid change blocks",
                Some(100),
                &[(Some(100), true), (Some(200), false)],
            ),
            (
                "latched: back to original still blocked",
                Some(100),
                &[(Some(200), false), (Some(100), false)],
            ),
            (
                "latched: unknown current still blocked",
                Some(100),
                &[(Some(200), false), (None, false)],
            ),
            (
                "unknown session pid fails open",
                None,
                &[(Some(100), true), (Some(200), true)],
            ),
            (
                "unknown current pid fails open (transient AX failure)",
                Some(100),
                &[(None, true), (Some(100), true)],
            ),
        ];
        for (label, session_pid, steps) in cases {
            let mut guard = FocusGuard::new();
            guard.session_started(*session_pid);
            for (i, (current, expected)) in steps.iter().enumerate() {
                assert_eq!(
                    guard.should_inject(*current),
                    *expected,
                    "{label}: step {i} current={current:?}"
                );
            }
        }
    }

    /// 新 session 必須解除上一輪的閂鎖（`session_started` 重置 latch 與基準 PID）；
    /// `latched()` 忠實反映閂鎖狀態（呼叫端據此做每 session 一次的警告）。
    #[test]
    fn focus_guard_new_session_resets_latch() {
        let mut guard = FocusGuard::new();
        guard.session_started(Some(100));
        assert!(!guard.latched(), "fresh session starts unlatched");
        assert!(!guard.should_inject(Some(200)), "first session latches");
        assert!(guard.latched(), "pid mismatch latches");
        guard.session_started(Some(200));
        assert!(!guard.latched(), "new session resets latch");
        assert!(
            guard.should_inject(Some(200)),
            "new session with new baseline injects again"
        );
    }
}
