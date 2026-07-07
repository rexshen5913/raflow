//! Whisper.cpp 終校後端（Phase 5），feature-gated by `whisper`。
//!
//! 完整規格見 `docs/spec/whisper.md`。本檔做最小封裝：
//! - 載入指定路徑的 ggml `.bin`（CoreML encoder 需與 .bin 同目錄、命名為 `*-encoder.mlmodelc/`）
//! - 餵 `&[i16]` (16 kHz mono) 跑一次 batch，回傳串接後的 transcript

use std::path::{Path, PathBuf};

use raflow_core::RaflowError;

/// Whisper 在 zh tokenizer 下對 dictation 命令字（「逗點」「句點」「換行」等）的處理常常壞掉：
///   1. **冗餘**：自己根據語意加標點，又把字本身音譯出來 → 「,逗點」
///   2. **後綴**：命令字後 Whisper 又加標點 → 「逗點，」
///   3. **音譯錯**：相近發音字混淆 → 「鬥點」「聚點」「緩行」
///   4. **連 dictation**：使用者連續說多個命令字 → 「鬥點，聚點，換行。」
///
/// 算法（遮罩式，四階段）——關鍵：區分「使用者命令產生的標點」與「Whisper 自己
/// 加的標點」。前者是使用者刻意 dictate 的（如「逗點 換行」要出「，\n」），
/// **不可**被 \n 吸收；後者是換行兩側的殘留（如「換行。」），要吸收。
///   - Pass A（遮罩）：變體 → 對應 PUA 遮罩字元（U+E000..），暫時不是真標點
///   - Pass B（collapse）：連續同類「原生」標點（半/全形混合）→ 單一全形
///   - Pass C（吸收）：對每個遮罩換行，吃掉兩側的空白 + 最多 1 個**原生**標點
///     （遮罩字元不吃 → 連續 dictation「逗點 換行」的逗點保留）
///   - Pass D（還原）：遮罩 → canonical，再 collapse 一次（命令標點與 Whisper
///     重複標點如「，，」合併）
///
/// 對 Apple Speech 已正確處理 dictation 的情況無副作用，因為 Apple 不會吐出
/// 這些命令字本身。
pub fn normalize_dictation_commands(text: &str) -> String {
    // (canonical, PUA 遮罩, 變體含同音 hallucination)
    const COMMANDS: &[(&str, char, &[&str])] = &[
        (
            "，",
            '\u{E001}',
            &[
                "逗點", "逗號", "鬥點", "鬥號", "豆點", "豆號", "都點", "抖點", "逗典", "兜點",
            ],
        ),
        (
            "。",
            '\u{E002}',
            &["句點", "句號", "聚點", "聚號", "據點", "巨點"],
        ),
        ("？", '\u{E003}', &["問號"]),
        ("！", '\u{E004}', &["驚嘆號", "感嘆號", "京嘆號", "趕嘆號"]),
        ("：", '\u{E005}', &["冒號"]),
        ("；", '\u{E006}', &["分號", "份號"]),
        ("、", '\u{E007}', &["頓號"]),
        (
            "\n",
            NEWLINE_MASK,
            &["換行", "新一行", "新行", "喚行", "緩行", "荒航", "換航"],
        ),
    ];
    // Pass A：遮罩
    let mut out = text.to_string();
    for (_, mask, variants) in COMMANDS {
        for variant in *variants {
            out = out.replace(variant, &mask.to_string());
        }
    }
    // Pass B + C
    let absorbed = absorb_around_newlines(&collapse_same_kind(&out));
    // Pass D：還原 + 終 collapse + 全形標點前空白清理（Whisper 分段殘留的
    // 「Teraphone ，」→「Teraphone，」；全形標點前的半形空白必為 junk）
    let mut unmasked = absorbed;
    for (canonical, mask, _) in COMMANDS {
        unmasked = unmasked.replace(*mask, canonical);
    }
    let mut out = collapse_same_kind(&unmasked);
    for punct in ["，", "。", "？", "！", "：", "；", "、"] {
        let spaced = format!(" {punct}");
        while out.contains(&spaced) {
            out = out.replace(&spaced, punct);
        }
    }
    out
}

/// 「換行」命令的 PUA 遮罩字元（吸收階段以此辨識命令換行；還原階段換回 `\n`）。
const NEWLINE_MASK: char = '\u{E000}';

/// 連續同類標點（含半/全形混合）縮成單一全形。Loop until fixpoint。
fn collapse_same_kind(text: &str) -> String {
    const SAME_KIND: &[(&[&str], &str)] = &[
        (&[",,", ",，", "，,", "，，"], "，"),
        (&["..", ".。", "。.", "。。"], "。"),
        (&["??", "?？", "？?", "？？"], "？"),
        (&["!!", "!！", "！!", "！！"], "！"),
        (&["::", ":：", "：:", "：："], "："),
        (&[";;", ";；", "；;", "；；"], "；"),
        (&["、、"], "、"),
    ];
    let mut out = text.to_string();
    loop {
        let mut changed = false;
        for (finds, replace) in SAME_KIND {
            for find in *finds {
                if out.contains(find) {
                    out = out.replace(find, replace);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// 對每個遮罩換行（[`NEWLINE_MASK`]），吃掉兩側的空白與「Whisper 殘留」標點：
/// - **後側**：空白* + ≤1 原生標點 + 空白*（「換行。」的殘留句點）。
/// - **前側**：空白*；原生標點只在「其前一字元是命令遮罩 / 位於字串開頭」時才吃
///   （命令與命令之間的分隔逗點必為 Whisper 殘留）。**字後的標點一律保留**——
///   Whisper 會把使用者說的「逗點」直接轉成「，」，與刻意 dictate 無法區分，
///   吃掉會刪到使用者要的標點（實測「Teraphone ， 換航」的逗點被吃）。
/// - 命令產生的標點此刻仍是 PUA 遮罩字元，永不被吃 →「逗點 換行」保留「，\n」。
///
/// 用 char 迭代避免 String::replace 鏈式吸收（會把不相關的標點也吃掉）。
fn absorb_around_newlines(text: &str) -> String {
    fn is_punct(c: char) -> bool {
        matches!(
            c,
            '，' | ',' | '。' | '.' | '？' | '?' | '！' | '!' | '：' | ':' | '；' | ';' | '、'
        )
    }
    fn is_mask(c: char) -> bool {
        ('\u{E000}'..='\u{E007}').contains(&c)
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == NEWLINE_MASK {
            // 前側：空白*；標點僅限「前一字元為遮罩 / 開頭」（命令間殘留分隔）
            while matches!(out.last(), Some(&' ')) {
                out.pop();
            }
            if matches!(out.last(), Some(&last) if is_punct(last)) {
                let before_punct = out.len().checked_sub(2).map(|j| out[j]);
                if before_punct.is_none_or(is_mask) {
                    out.pop();
                }
            }
            while matches!(out.last(), Some(&' ')) {
                out.pop();
            }
            out.push(NEWLINE_MASK);
            // 後側：空白* + ≤1 原生標點 + 空白*
            let mut j = i + 1;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            if j < chars.len() && is_punct(chars[j]) {
                j += 1;
            }
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out.into_iter().collect()
}

/// Whisper 校正結果 safety check：
/// Whisper 在 zh tokenizer 下偶爾會冒出韓文、日文假名、西里爾等非預期字元（尤其
/// 短音節 + 模糊發音時）；這些情境下 Whisper 結果反而比 Apple final 更糟，呼叫端
/// 應該直接 fallback 到 Apple final。
///
/// 允許的字元範圍（針對 zh-TW + 英數混排場景）：
/// - ASCII（U+0000..U+007F）：英文 / 數字 / 標點 / 空白 / 換行
/// - CJK Unified Ideographs（U+4E00..U+9FFF）：中文漢字（繁體簡體共用區）
/// - CJK Symbols and Punctuation（U+3000..U+303F）：「」、。…
/// - General Punctuation（U+2000..U+206F）：— … ' ' " "
/// - Halfwidth and Fullwidth Forms（U+FF00..U+FFEF）：全形 ＡＢ１２，！
/// - CJK Compatibility（U+F900..U+FAFF）：罕用漢字相容區
///
/// 禁：Hangul（U+AC00..U+D7AF / U+1100..U+11FF）、Hiragana（U+3040..U+309F）、
/// Katakana（U+30A0..U+30FF）、Cyrillic、Arabic 等。
pub fn is_safe_whisper_output(text: &str) -> bool {
    text.chars().all(|c| {
        matches!(c as u32,
            0x0000..=0x007F  // ASCII
            | 0x2000..=0x206F  // General Punctuation
            | 0x3000..=0x303F  // CJK Symbols and Punctuation
            | 0x4E00..=0x9FFF  // CJK Unified Ideographs
            | 0xF900..=0xFAFF  // CJK Compatibility
            | 0xFF00..=0xFFEF  // Halfwidth and Fullwidth Forms
        )
    })
}

/// 滾動游標穩定性（Phase 2 fix，spec/whisper.md §15）：把 VAD 段列表依「已定稿樣本
/// 位置」cutoff 過濾——完全在 cutoff 前的段剔除、跨界段裁到 cutoff 起（跨界只會來自
/// VAD pad 回溯或重切合併，裁掉部分屬已定稿音訊，無害）、之後的段原樣。
///
/// 動機：VAD 對成長中的緩衝每 tick 重切，段的合併/分裂會讓「段數游標」指錯音訊 →
/// 已定稿內容被重複轉錄（實測「另外我們」重複、尾句 ×4）。樣本位置 cutoff 單調遞增，
/// 不受重切影響。
pub fn segments_pending(
    segments: &[std::ops::Range<usize>],
    finalized_samples: usize,
) -> Vec<std::ops::Range<usize>> {
    segments
        .iter()
        .filter_map(|seg| {
            let start = seg.start.max(finalized_samples);
            (start < seg.end).then_some(start..seg.end)
        })
        .collect()
}

/// 重複迴圈幻覺守門（Phase 2 fix）：Whisper decoder 重複迴圈會吐「同一句 ×N」
/// （純中文可通過 `is_safe_whisper_output` 的字元集檢查）。同一 ≥3 字元單元**連續**
/// 出現 ≥3 次視為退化輸出；單元下限 3（總長 ≥9）避免誤殺日常疊字（好好好、
/// 哈哈哈哈——單元 1~2 不觸發）；實測「觀看 觀看 觀看 觀看」單元含空白為 3。
/// 呼叫端（rolling）拒收後不推進游標，下輪以更長上下文重試。
pub fn is_repetition_loop(text: &str) -> bool {
    const MIN_UNIT: usize = 3;
    const MIN_REPEATS: usize = 3;
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    if n < MIN_UNIT * MIN_REPEATS {
        return false;
    }
    // 對每個單元長度，檢查是否存在起點 i 使 unit 連續出現 MIN_REPEATS 次。
    for unit in MIN_UNIT..=n / MIN_REPEATS {
        for i in 0..=n - unit * MIN_REPEATS {
            let first = &chars[i..i + unit];
            if (1..MIN_REPEATS).all(|k| &chars[i + k * unit..i + (k + 1) * unit] == first) {
                return true;
            }
        }
    }
    false
}

/// Prompt priming 術語上限：prompt 過長會稀釋偏置效果、消耗 context（whisper.cpp 慣例 prompt
/// 額度 ~`n_text_ctx/2` ≈ 224 token）、並擴大回吐（prompt echo）面積。英文術語每個約 2~4 token
/// （含頓號分隔），30 個約 ~100~130 token，仍穩在額度內；50~60 個起才逼近上限。
pub const ROLLING_PROMPT_TERM_CAP: usize = 30;
/// 內建 dictation 命令字：讓 Whisper 把口述的「逗點/句點/換行」拼成標準形
/// （而非 多點/去點/萬行 等發音近似變體），交給 normalize 收斂為標點。
const ROLLING_PROMPT_COMMANDS: &[&str] = &["逗點", "句點", "換行"];

/// Prompt 的 scaffold 前綴：回吐時最容易整句抄出的特徵字串（守門用）。
const ROLLING_PROMPT_SCAFFOLD: &str = "詞彙：";

/// 組滾動路徑的 initial_prompt：`詞彙：A、B、…、逗點、句點、換行。`
/// 用中文頓號分隔（以 ", " 分隔的列表實測會把 ASCII 逗號風格滲進輸出：「CI, CD」
/// 「Argo CD,。」）；「詞彙：」scaffold 是回吐守門的特徵錨點。
pub fn build_rolling_prompt(terms: &[&str]) -> String {
    let parts: Vec<&str> = terms
        .iter()
        .copied()
        .take(ROLLING_PROMPT_TERM_CAP)
        .chain(ROLLING_PROMPT_COMMANDS.iter().copied())
        .collect();
    format!("{ROLLING_PROMPT_SCAFFOLD}{}。", parts.join("、"))
}

/// Prompt 回吐守門（防範 ADR-0006 記錄的已知幻覺模式）：輸出含 scaffold（「詞彙：」）或 prompt 的
/// **相鄰詞對「A、B」格式** → 判定為把 prompt 抄成輸出。單一術語出現是**目的**
/// （不算回吐）；成對且帶 prompt 的分隔格式才是抄。
pub fn is_prompt_echo(text: &str, prompt_terms: &[&str]) -> bool {
    if text.contains(ROLLING_PROMPT_SCAFFOLD) {
        return true;
    }
    let capped: Vec<&str> = prompt_terms
        .iter()
        .copied()
        .take(ROLLING_PROMPT_TERM_CAP)
        .chain(ROLLING_PROMPT_COMMANDS.iter().copied())
        .collect();
    capped
        .windows(2)
        .any(|pair| text.contains(&format!("{}、{}", pair[0], pair[1])))
}

/// 滾動鎖定**基礎**門檻：最後語音段結束後需累積的靜音樣本數（2.0s @ 16 kHz）。
/// 須大於實測句內思考停頓（~1.6s）——但真人錄音實證句間停頓僅 1.17~1.39s，單一門檻
/// 無法兩全 → 以 [`rolling_trailing_silence_for`] 依累積語音長度自適應調降。
pub const ROLLING_TRAILING_SILENCE_SAMPLES: usize = 32_000;
/// 快速鎖定門檻（1.0s）：未定稿語音已累積夠長時採用。搭配 1s tick 節奏，句內
/// ≤0.95s 的小停頓在下個 tick 前必被新語音填補，不會誤觸。
pub const ROLLING_TRAILING_SILENCE_FAST: usize = 16_000;
/// 「未定稿語音累積達此量（5s）→ 改用快速門檻」。一句講到 5 秒以上，任何像樣的
/// 停頓都是可信的句界；反之剛開口的短內容用基礎門檻，防句內思考停頓提前鎖
/// （短片段轉錄品質差、且 committed 邊界近似誤差大）。
pub const ROLLING_PENDING_SPEECH_FAST_LOCK: usize = 80_000;
/// 「未定稿語音低於此量（1.5s）→ 永不因靜音鎖定」。真人錄音實測：孤兒短片段
/// （「換行 再來」1.04s）在長停頓後單獨鎖定 → 無上下文轉出「和一半」等退化輸出。
/// 留給下一句併入（更多上下文）或 is_final 收尾（不看門檻）處理。
pub const ROLLING_MIN_LOCK_SPEECH: usize = 24_000;

/// 依「未定稿語音累積樣本數」回傳鎖定門檻（自適應三段，真人錄音實證定調——
/// 句間停頓 1.17~1.39s vs 句內思考停頓 1.6s 重疊，單一門檻無法分離；孤兒短片段
/// 單獨鎖定必產生退化輸出。見上方常數 doc）。`usize::MAX` = 本 tick 永不因靜音鎖定。
pub fn rolling_trailing_silence_for(pending_speech_samples: usize) -> usize {
    if pending_speech_samples < ROLLING_MIN_LOCK_SPEECH {
        usize::MAX
    } else if pending_speech_samples >= ROLLING_PENDING_SPEECH_FAST_LOCK {
        ROLLING_TRAILING_SILENCE_FAST
    } else {
        ROLLING_TRAILING_SILENCE_SAMPLES
    }
}

/// [`rolling_tick_core`] 單次 tick 的結果。
pub struct RollingTickOutcome {
    /// 本 tick 鎖定的句子（已簡→繁 + dictation 正規化）；無鎖定 → `None`。
    pub phrase: Option<String>,
    /// 被守門拒收的原始輸出（診斷用；不注入、游標不推進，下輪更長上下文重試）。
    pub rejected: Vec<String>,
    /// 新游標（已定稿音訊結束樣本位置）。僅在鎖定時前進，否則等於輸入值。
    pub finalized_samples: usize,
    /// 本 tick 偵測到的未定稿語音量（samples）。收尾 flush 用
    /// [`rolling_final_flush_delivered`] 判定是否需要回退 Apple final。
    pub pending_speech_samples: usize,
}

/// 收尾 flush（`is_final=true`）是否已把 pending 語音全部交付：有鎖定句、或本來
/// 就無 pending 語音 → 可安全送空 `Final` 觸發剪貼簿；否則呼叫端必須放行 Apple
/// final 作回退——空 `Final` 會讓 printer 把未定稿草稿整段 backspace 清掉
/// （守門拒收 / Whisper 空輸出 / VAD 失敗時的資料遺失路徑）。
pub fn rolling_final_flush_delivered(phrase_locked: bool, pending_speech_samples: usize) -> bool {
    phrase_locked || pending_speech_samples == 0
}

/// Phase 2 句級滾動**決策核心**（ADR-0006 §8.7.2）——`AppleSpeechBackend::rolling_tick`
/// 與離線 `rolling_harness`（examples/）共用同一份邏輯，離線驗證即等於驗證 app 行為。
///
/// 步驟：VAD 只掃游標後音訊（§16.3）→ `segments_pending` 過濾 → `segments_ready_to_finalize`
/// 停頓即鎖（門檻依累積語音長度自適應，[`rolling_trailing_silence_for`]）→ 串接語音段
/// 直送 `transcribe_span` → 守門（字元集 §12 + 重複迴圈）→ 簡→繁 + dictation 正規化 →
/// 游標推進（僅鎖定時）。
///
/// 呼叫端負責：PCM 快照、送 `PhraseFinal` / 收尾 `Final`、記 log、持有游標狀態。
/// `prompt_terms`：`Some` 時以 [`build_rolling_prompt`]（術語 + 命令字）作為
/// initial_prompt 引導拼寫，並以 [`is_prompt_echo`] 守門回吐；`None` 時不帶 prompt。
pub fn rolling_tick_core(
    whisper: &WhisperContext,
    vad_model_path: &Path,
    pcm: &[i16],
    finalized_samples: usize,
    is_final: bool,
    prompt_terms: Option<&[&str]>,
) -> Result<RollingTickOutcome, RaflowError> {
    let mut out = RollingTickOutcome {
        phrase: None,
        rejected: Vec::new(),
        finalized_samples,
        pending_speech_samples: 0,
    };
    // VAD 只掃游標之後的音訊；相對範圍 +offset 還原絕對樣本位置。
    let offset = finalized_samples.min(pcm.len());
    let segments = whisper.speech_segments(&pcm[offset..], vad_model_path)?;
    let segments: Vec<std::ops::Range<usize>> = segments
        .into_iter()
        .map(|r| r.start + offset..r.end + offset)
        .collect();
    // 游標穩定性：切片後多為 no-op，防禦 VAD pad 回溯（見 segments_pending doc）。
    let pending = segments_pending(&segments, finalized_samples);
    // 自適應門檻：未定稿語音累積越長，鎖定所需的段末靜音越短（真人節奏實證）。
    let pending_speech: usize = pending.iter().map(|r| r.end - r.start).sum();
    out.pending_speech_samples = pending_speech;
    let segment_ends: Vec<usize> = pending.iter().map(|r| r.end).collect();
    let range = segments_ready_to_finalize(
        &segment_ends,
        pcm.len(),
        0, // pending 已排除定稿段 → 游標恆從 0 起
        rolling_trailing_silence_for(pending_speech),
        is_final,
    );
    if range.is_empty() {
        return Ok(out);
    }
    // 整句一起轉錄：串接 range 內語音段直送（免二次 VAD；Whisper 拿到完整句不掉字）。
    let mut span: Vec<i16> = Vec::new();
    for seg in &pending[range.clone()] {
        span.extend_from_slice(&pcm[seg.clone()]);
    }
    let prompt = prompt_terms.map(build_rolling_prompt);
    let t = whisper.transcribe_span(&span, prompt.as_deref())?;
    if t.trim().is_empty() {
        // VAD 濾光 / 無語音 → 跳過（不送空 PhraseFinal）。
        return Ok(out);
    }
    if !is_safe_whisper_output(&t)
        || is_repetition_loop(&t)
        || prompt_terms.is_some_and(|terms| is_prompt_echo(&t, terms))
    {
        // 幻覺守門：非 zh/en 字元集（§12）、重複迴圈、或 prompt 回吐 → 拒收，游標不推進。
        out.rejected.push(t);
        return Ok(out);
    }
    // 先簡→繁（zh-TW），再正規化 dictation 命令字（後者比對繁體變體）。
    out.phrase = Some(normalize_dictation_commands(&whisper.to_traditional(&t)));
    out.finalized_samples = pending[range.end - 1].end;
    Ok(out)
}

/// whisper.cpp encoder：1 個 audio-ctx token = 20ms = 320 samples @16kHz（30s 視窗
/// = 480_000 samples = 1500 tokens）。
#[cfg(any(feature = "whisper", test))]
const SAMPLES_PER_AUDIO_CTX_TOKEN: usize = 320;
/// encoder 滿視窗 tokens（30s）。`audio_ctx` 上限；超長音訊由 `full()` 內部分窗。
#[cfg(any(feature = "whisper", test))]
const AUDIO_CTX_MAX: i32 = 1_500;
/// `audio_ctx` 下限：**離線 harness 實證**（rolling_harness）——下限 256 會讓
/// turbo 輸出退化——句內 ×2 重複（「接下來是第二句話」×2）與後半截斷（「…會使用」
/// 掉尾）；512 全場景乾淨且比滿視窗快 2~3 倍（鎖定 ~350-480ms vs ~1050ms）。
#[cfg(any(feature = "whisper", test))]
const AUDIO_CTX_MIN: i32 = 512;
/// 換算後的安全餘裕 tokens（避免語音貼齊視窗邊緣被截斷）。
#[cfg(any(feature = "whisper", test))]
const AUDIO_CTX_MARGIN: i32 = 64;

/// 依音訊樣本數換算 `FullParams::set_audio_ctx` 的 encoder 視窗 tokens（延遲優化，
/// spec/whisper.md §16）：`clamp(ceil(samples/320) + 餘裕, 下限, 1500)`。
/// whisper.cpp 對任意長度輸入都補零到 30s 跑滿 encoder；turbo 的 encoder 是 large 級，
/// 對滾動尾句（2~5s）是每次 ~10 倍的固定浪費——裁到實際長度即省。
#[cfg(any(feature = "whisper", test))]
pub fn audio_ctx_for_samples(samples: usize) -> i32 {
    let needed = samples.div_ceil(SAMPLES_PER_AUDIO_CTX_TOKEN);
    let needed = i32::try_from(needed).unwrap_or(AUDIO_CTX_MAX);
    (needed.saturating_add(AUDIO_CTX_MARGIN)).clamp(AUDIO_CTX_MIN, AUDIO_CTX_MAX)
}

/// 模型偏好序（前者優先）：large-v3-turbo q5_0（術語辨識準、Metal GPU；
/// `make whisper-model-turbo` 下載）> small。詳見 spec/whisper.md §4。
const MODEL_PREFERENCE: &[&str] = &["ggml-large-v3-turbo-q5_0.bin", "ggml-small.bin"];

/// 在 `models_dir` 依 `MODEL_PREFERENCE` 挑第一個**存在**的模型檔。
/// 都不存在時回傳 small 路徑（維持原「檔案缺失 → startup log 提示並回退
/// Apple-only」的行為與錯誤訊息）。
pub fn preferred_model_in(models_dir: &Path) -> PathBuf {
    for name in MODEL_PREFERENCE {
        let candidate = models_dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    models_dir.join("ggml-small.bin")
}

/// 預設 model 路徑：`$HOME/Library/Application Support/raflow/models/` 內依
/// `MODEL_PREFERENCE` 偏好序自動挑選（turbo 存在即用 turbo，否則 small）。
/// 由 env `RAFLOW_WHISPER_MODEL` 覆寫。
pub fn default_model_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("raflow");
    p.push("models");
    Some(preferred_model_in(&p))
}

/// 解析「使用者要的 model 路徑」：env 優先；fallback 預設位置。
pub fn resolve_model_path() -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("RAFLOW_WHISPER_MODEL") {
        return Some(PathBuf::from(env));
    }
    default_model_path()
}

/// 預設 Silero VAD model 路徑：
/// `$HOME/Library/Application Support/raflow/models/ggml-silero-v6.2.0.bin`。
/// 由 env `RAFLOW_VAD_MODEL` 覆寫。Phase 1（抗幻覺串流）用；檔名沿用上游官方發佈名。
pub fn default_vad_model_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("raflow");
    p.push("models");
    p.push("ggml-silero-v6.2.0.bin");
    Some(p)
}

/// 解析 VAD model 路徑：env `RAFLOW_VAD_MODEL` 優先；fallback 預設位置。
pub fn resolve_vad_model_path() -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("RAFLOW_VAD_MODEL") {
        return Some(PathBuf::from(env));
    }
    default_vad_model_path()
}

/// Phase 2 滾動切句決策核心（純函式，ADR-0006 §8.7.2「停頓即鎖，整句一起定稿」）。
///
/// 每個計時 tick 對 buffer 重跑 VAD，得到各語音段的**結束取樣索引** `segment_ends`（遞增、
/// 已 clamp 到 `buffer_len`），其中前 `finalized` 段已定稿。回傳本輪應**新定稿**的段索引範圍
/// `[finalized, end)`。
///
/// **關鍵**：只在**最後一段結束後靜音 ≥ `trailing_silence_samples`**（使用者停頓）時，才把
/// 「自上次定稿以來累積的所有段」**一起**定稿（`[finalized, total)`）——講話中（含句內小停頓
/// 造成的分段）都不定稿。呼叫端據此把整段當「一句」餵給 Whisper（完整上下文，避免逐小段掉字）。
/// 這修掉了舊版「非最後段即刻逐段定稿」導致句子被句內停頓切碎、Whisper 短片段掉字的問題。
/// 錄音停止（`is_final = true`）：剩餘段全部定稿。
///
/// 保證範圍不越界、不回頭（`start <= end <= segment_ends.len()`）。
pub fn segments_ready_to_finalize(
    segment_ends: &[usize],
    buffer_len: usize,
    finalized: usize,
    trailing_silence_samples: usize,
    is_final: bool,
) -> std::ops::Range<usize> {
    let total = segment_ends.len();
    // 「可定稿」＝停止收尾，或有語音段且最後一段後已有足夠靜音（使用者停頓）。
    let ready = is_final
        || (total > 0
            && buffer_len.saturating_sub(segment_ends[total - 1]) >= trailing_silence_samples);
    let end = if ready { total } else { finalized.min(total) };
    // finalized 可能 > end（防禦性 clamp；不回頭、不越界）。
    let start = finalized.min(end);
    start..end
}

/// VAD 段時間戳單位為 centisecond（10 ms）；轉 16 kHz 取樣索引 = cs × 160。
/// 純算術，不依賴 whisper feature，供 [`vad_cs_to_samples`] 與 FFI 段擷取共用。
pub const SAMPLES_PER_CENTISECOND: f32 = 160.0;

/// Phase 2 滾動 pipeline 開關 `RAFLOW_ROLLING` 的純解析（預設 ON，經實機驗證後轉正）。
///
/// 語意：空白 / `"0"` / `"false"`（不分大小寫）→ OFF；未設或其餘任何值 → ON。
/// 抽成純函式讓測試不碰 process 全域 env（憲法 §2.4）；env 讀取見 [`rolling_enabled`]。
pub fn parse_rolling_flag(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(v) => {
            let v = v.trim();
            !(v.is_empty() || v == "0" || v.eq_ignore_ascii_case("false"))
        }
    }
}

/// 讀 `RAFLOW_ROLLING` env 決定是否啟用句級滾動 pipeline（預設 ON；`0`/`false` 關閉）。
/// 薄層包裝 [`parse_rolling_flag`]（後者已測）；本函式只做 env I/O，不另測。
pub fn rolling_enabled() -> bool {
    parse_rolling_flag(std::env::var("RAFLOW_ROLLING").ok().as_deref())
}

/// 把一個 VAD 語音段的 centisecond 時間戳轉成 16 kHz 取樣索引範圍（ADR-0006 §2.3 段擷取）。
///
/// - `start_cs` / `end_cs`：VAD 回傳的段起訖（centisecond，×160 → sample）。
/// - clamp 到 `[0, total_samples]`；`start > end`（VAD 異常）→ 收斂為空範圍，
///   保證 `&audio[range]` 不會 panic（`start <= end <= total_samples`）。
pub fn vad_cs_to_samples(
    start_cs: f32,
    end_cs: f32,
    total_samples: usize,
) -> std::ops::Range<usize> {
    let start = (start_cs * SAMPLES_PER_CENTISECOND).max(0.0) as usize;
    let end = ((end_cs * SAMPLES_PER_CENTISECOND).max(0.0) as usize).min(total_samples);
    let start = start.min(end);
    start..end
}

/// ADR-0006 §2.2 cumulative 前綴裁切：Apple partial 為整段累積文字，送 printer 前先剝掉
/// 「已鎖定前綴」，只留當前未定稿句的草稿。
///
/// - `committed_chars`：定稿當下 cumulative 的 char 數（已鎖定前綴長度）。
/// - 以 `char`（Unicode scalar value）為單位，與 `raflow-input` 的 `compute_stream_diff` 對齊。
/// - `committed_chars ≥` 文字長度 → 回空字串（Apple 偶爾回改前綴變短的防禦；下個
///   `PhraseFinal` 會重新對齊，不影響已鎖定內容）。
pub fn strip_committed_prefix(cumulative: &str, committed_chars: usize) -> String {
    cumulative.chars().skip(committed_chars).collect()
}

#[cfg(not(feature = "whisper"))]
mod stub {
    use super::*;

    /// 無 feature 時的 stub：構造永遠失敗，呼叫端會自動退化為 Apple-only mode。
    pub struct WhisperContext;

    impl WhisperContext {
        pub fn load(path: &Path, _language: &str) -> Result<Self, RaflowError> {
            Err(RaflowError::WhisperLoad {
                detail: format!(
                    "raflow-speech compiled without `whisper` feature; cannot load {}",
                    path.display()
                ),
            })
        }

        pub fn transcribe(&self, _pcm_i16: &[i16]) -> Result<String, RaflowError> {
            Err(RaflowError::WhisperInference {
                detail: "compiled without `whisper` feature".into(),
            })
        }

        pub fn transcribe_streaming(
            &self,
            _pcm_i16: &[i16],
            _vad_model_path: &Path,
        ) -> Result<String, RaflowError> {
            Err(RaflowError::WhisperInference {
                detail: "compiled without `whisper` feature".into(),
            })
        }

        pub fn transcribe_span(
            &self,
            _pcm_i16: &[i16],
            _initial_prompt: Option<&str>,
        ) -> Result<String, RaflowError> {
            Err(RaflowError::WhisperInference {
                detail: "compiled without `whisper` feature".into(),
            })
        }

        pub fn speech_segments(
            &self,
            _pcm_i16: &[i16],
            _vad_model_path: &Path,
        ) -> Result<Vec<std::ops::Range<usize>>, RaflowError> {
            Err(RaflowError::WhisperInference {
                detail: "compiled without `whisper` feature".into(),
            })
        }

        pub fn to_traditional(&self, text: &str) -> String {
            text.to_string()
        }
    }
}

#[cfg(feature = "whisper")]
mod imp {
    use super::*;
    use ferrous_opencc::OpenCC;
    use ferrous_opencc::config::BuiltinConfig;
    use std::sync::Mutex;
    use whisper_rs::{
        FullParams, SamplingStrategy, WhisperContext as WhisperRsContext, WhisperContextParameters,
        WhisperState, WhisperVadContext, WhisperVadContextParams, WhisperVadParams,
        convert_integer_to_float_audio,
    };

    /// 把 VAD 語音段範圍對應的樣本串接回傳（16 kHz f32）。空段 → 空 `Vec`。
    fn concat_speech(ranges: &[std::ops::Range<usize>], audio_f32: &[f32]) -> Vec<f32> {
        let mut speech = Vec::new();
        for range in ranges {
            speech.extend_from_slice(&audio_f32[range.clone()]);
        }
        speech
    }

    pub struct WhisperContext {
        ctx: WhisperRsContext,
        language: String,
        // 快取的推論 state（KV cache + Metal buffers；turbo 建立為百毫秒級）——跨呼叫
        // 重用免每次重付（延遲優化，spec §16）。Mutex 同時充當推論序列化鎖（原
        // state_lock 職責）：Apple final callback 與 worker rolling_tick 都可能進來。
        state: Mutex<Option<WhisperState>>,
        // 注意：Silero VAD context **刻意不快取**——實測重用的
        // context 每呼叫一次就變慢（61k 樣本從 ~80ms 劣化到 1566ms，與樣本數無關的
        // 單調增長），而重建很便宜（含載入 ~1MB 模型僅數 ms）。每次呼叫新建。
        // 句級滾動輸出簡→繁（zh-TW，S2TWP 含詞彙轉換）。載入失敗 → None，to_traditional 原樣
        // 回傳（降級不阻擋 whisper 啟用）。字典內嵌於 crate，建立一次重用。
        opencc: Option<OpenCC>,
    }

    impl WhisperContext {
        pub fn load(path: &Path, language: &str) -> Result<Self, RaflowError> {
            if !path.exists() {
                return Err(RaflowError::WhisperModelMissing {
                    path: path.to_path_buf(),
                });
            }
            let path_str = path.to_str().ok_or_else(|| RaflowError::WhisperLoad {
                detail: format!("model path is not valid UTF-8: {}", path.display()),
            })?;
            // Metal 上 flash attention 對 encoder/decoder 皆有顯著加速（whisper.cpp
            // 官方建議 GPU 後端開啟）；與 DTW token timestamps 互斥，本專案不用 DTW。
            let mut ctx_params = WhisperContextParameters::default();
            ctx_params.flash_attn(true);
            let ctx = WhisperRsContext::new_with_params(path_str, ctx_params).map_err(|e| {
                RaflowError::WhisperLoad {
                    detail: format!("WhisperContext::new_with_params: {e}"),
                }
            })?;
            // 簡→繁（Taiwan + 詞彙）轉換器；建立失敗不阻擋 whisper（降級為不轉換）。
            let opencc = match OpenCC::from_config(BuiltinConfig::S2twp) {
                Ok(cc) => Some(cc),
                Err(e) => {
                    eprintln!(
                        "raflow: OpenCC 簡→繁初始化失敗，滾動輸出將維持原樣（可能簡體）: {e}"
                    );
                    None
                }
            };
            Ok(Self {
                ctx,
                language: language.to_string(),
                state: Mutex::new(None),
                opencc,
            })
        }

        /// 簡→繁轉換（zh-TW，S2TWP 含詞彙如 软件→軟體、程序→程式）。句級滾動路徑用：
        /// `transcribe_streaming` 不帶 initial_prompt 故 Whisper 預設吐簡體，以此後轉換補繁體
        /// （確定性、零幻覺風險）。轉換器缺席（載入失敗）→ 原樣回傳。
        pub fn to_traditional(&self, text: &str) -> String {
            match &self.opencc {
                Some(cc) => cc.convert(text),
                None => text.to_string(),
            }
        }

        /// 取得（惰性建立並快取的）推論 state。首次呼叫配置 KV cache + Metal buffers
        /// （turbo 為百毫秒級），之後重用；Mutex guard 同時序列化推論。
        fn lock_state(
            &self,
        ) -> Result<std::sync::MutexGuard<'_, Option<WhisperState>>, RaflowError> {
            let mut guard = self
                .state
                .lock()
                .map_err(|_| RaflowError::WhisperInference {
                    detail: "state lock poisoned".into(),
                })?;
            if guard.is_none() {
                let state = self
                    .ctx
                    .create_state()
                    .map_err(|e| RaflowError::WhisperInference {
                        detail: format!("create_state: {e}"),
                    })?;
                *guard = Some(state);
            }
            Ok(guard)
        }

        /// 對 `audio_f32` 跑 Silero VAD，回傳語音段**取樣索引範圍**（16 kHz，
        /// 已 clamp、濾掉空段）。無語音段（靜音 / 雜訊）→ 空 `Vec`。
        /// context **每次新建**（重用會單調劣化，見 struct 欄位註解；重建僅數 ms）。
        fn vad_ranges(
            &self,
            vad_model_path: &Path,
            audio_f32: &[f32],
        ) -> Result<Vec<std::ops::Range<usize>>, RaflowError> {
            let path_str = vad_model_path
                .to_str()
                .ok_or_else(|| RaflowError::WhisperLoad {
                    detail: format!(
                        "VAD model path is not valid UTF-8: {}",
                        vad_model_path.display()
                    ),
                })?;
            let mut vad_ctx = WhisperVadContext::new(path_str, WhisperVadContextParams::default())
                .map_err(|e| RaflowError::WhisperInference {
                    detail: format!("VAD init: {e}"),
                })?;

            let mut vad_params = WhisperVadParams::new();
            vad_params.set_speech_pad(200); // 段前後各留 200ms padding，避免切掉語音邊緣

            let segments = vad_ctx
                .segments_from_samples(vad_params, audio_f32)
                .map_err(|e| RaflowError::WhisperInference {
                    detail: format!("VAD segments_from_samples: {e}"),
                })?;

            let total = audio_f32.len();
            Ok(segments
                .into_iter()
                .map(|seg| super::vad_cs_to_samples(seg.start, seg.end, total))
                .filter(|r| r.start < r.end)
                .collect())
        }

        pub fn transcribe(&self, pcm_i16: &[i16]) -> Result<String, RaflowError> {
            if pcm_i16.is_empty() {
                return Ok(String::new());
            }
            let mut state_guard = self.lock_state()?;
            let state = state_guard
                .as_mut()
                .ok_or_else(|| RaflowError::WhisperInference {
                    detail: "whisper state unavailable".into(),
                })?;

            let mut audio_f32 = vec![0.0_f32; pcm_i16.len()];
            convert_integer_to_float_audio(pcm_i16, &mut audio_f32).map_err(|e| {
                RaflowError::WhisperInference {
                    detail: format!("convert_integer_to_float_audio: {e}"),
                }
            })?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some(self.language.as_str()));
            params.set_translate(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_print_special(false);
            // 強制繁體中文 + 英數混排：Whisper 在 zh tokenizer 預設輸出簡體；給一段
            // traditional Chinese 範例 prompt 引導它跟樣輸出繁體（這招對 zh-Hant /
            // zh-Hans 切換實測非常有效）。詳見 docs/spec/whisper.md §11。
            params.set_initial_prompt(
                "以下是繁體中文的逐字稿，可能夾雜英文技術術語：「我用 Cursor 打開這個 \
                 專案，然後 npm install 跑起來。」",
            );
            // state 跨呼叫重用（spec §16）：斷開殘留的上輪文字 context（prompt_past），
            // 維持與原本每次新建 state 一致的獨立性（initial_prompt 不受影響）。
            params.set_no_context(true);
            // encoder 視窗裁到實際音訊長度（短音訊免付 30s 滿視窗；≥30s 同原行為）。
            params.set_audio_ctx(audio_ctx_for_samples(audio_f32.len()));

            state
                .full(params, &audio_f32)
                .map_err(|e| RaflowError::WhisperInference {
                    detail: format!("state.full: {e}"),
                })?;

            let mut out = String::new();
            for segment in state.as_iter() {
                let text = segment
                    .to_str_lossy()
                    .map_err(|e| RaflowError::WhisperInference {
                        detail: format!("segment.to_str_lossy: {e}"),
                    })?;
                out.push_str(text.trim());
            }
            // 規範化 dictation 命令字（含夾心 / 前綴 / 後綴 / 裸字 + 同音字 hallucination 變體）
            Ok(super::normalize_dictation_commands(&out))
        }

        /// Phase 1 抗幻覺串流模式（見 `docs/design/next-mixed-zh-en-streaming.md`）。
        ///
        /// 與 [`transcribe`](Self::transcribe) 的差異，全為壓制 ADR-0006 的幻覺根因：
        /// - **VAD 前置過濾**：Silero VAD 先切掉靜音/雜訊段，只把語音段送進 encoder；
        /// - **`suppress_nst(true)`**：抑制 non-speech tokens；
        /// - **`temperature 0`**：不觸發 temperature fallback 的臆測解碼；
        /// - **不加 `initial_prompt`**：ADR-0006 主因——短片段會把 prompt 文字回吐成輸出。
        ///
        /// **實作重點（踩過的坑）**：whisper-rs 的 `state.full()` 綁定 `whisper_full_with_state`，
        /// 而該版 whisper.cpp 只有 top-level `whisper_full` 會套用 `params.vad`——換言之
        /// `FullParams::enable_vad` 在 state API 上是 no-op。故此處改用 standalone
        /// [`WhisperVadContext`] **自行**跑 VAD、擷取語音段、再把語音段餵給轉錄。
        /// 副產物是「明確的語音段時間戳」，正好是 Phase 2 rolling pipeline 需要的切句依據。
        ///
        /// 回傳**原始**串接 transcript（不做 dictation 正規化），讓 harness 能直接觀察
        /// Whisper 是否幻覺；正規化屬終校（Phase 2）關切，不在隔離實驗範圍。
        pub fn transcribe_streaming(
            &self,
            pcm_i16: &[i16],
            vad_model_path: &Path,
        ) -> Result<String, RaflowError> {
            if pcm_i16.is_empty() {
                return Ok(String::new());
            }
            if !vad_model_path.exists() {
                return Err(RaflowError::WhisperModelMissing {
                    path: vad_model_path.to_path_buf(),
                });
            }

            let mut audio_f32 = vec![0.0_f32; pcm_i16.len()];
            convert_integer_to_float_audio(pcm_i16, &mut audio_f32).map_err(|e| {
                RaflowError::WhisperInference {
                    detail: format!("convert_integer_to_float_audio: {e}"),
                }
            })?;

            // --- Stage 1：VAD 前置過濾，擷取語音段（見上方 doc comment 的坑）---
            let ranges = self.vad_ranges(vad_model_path, &audio_f32)?;
            let speech = concat_speech(&ranges, &audio_f32);
            if speech.is_empty() {
                // 靜音 / 雜訊被 VAD 濾光 → 空輸出（這正是關卡要驗證的行為）
                return Ok(String::new());
            }

            // --- Stage 2：只對語音段轉錄 ---
            self.full_streaming(&speech, None)
        }

        /// Phase 2 rolling 直送路徑（延遲優化，spec §16）：呼叫端（`rolling_tick`）已用
        /// [`speech_segments`](Self::speech_segments) 切好段並自行串接語音樣本 →
        /// 免 [`transcribe_streaming`](Self::transcribe_streaming) 內的**第二次 VAD**。
        /// 輸入必須是「只含語音」的 16 kHz mono 樣本（抗幻覺前提：靜音已被 VAD 濾除）。
        /// `initial_prompt`：`Some` = 術語/命令字 priming（回吐由呼叫端守門）。
        pub fn transcribe_span(
            &self,
            pcm_i16: &[i16],
            initial_prompt: Option<&str>,
        ) -> Result<String, RaflowError> {
            if pcm_i16.is_empty() {
                return Ok(String::new());
            }
            let mut audio_f32 = vec![0.0_f32; pcm_i16.len()];
            convert_integer_to_float_audio(pcm_i16, &mut audio_f32).map_err(|e| {
                RaflowError::WhisperInference {
                    detail: format!("convert_integer_to_float_audio: {e}"),
                }
            })?;
            self.full_streaming(&audio_f32, initial_prompt)
        }

        /// 串流抗幻覺推論核心（[`transcribe_streaming`](Self::transcribe_streaming) 與
        /// [`transcribe_span`](Self::transcribe_span) 共用）：對「已 VAD 過濾、只含語音」
        /// 的 f32 樣本跑一次 `full()`。temperature 0 + suppress_nst、不設 initial_prompt
        /// （ADR-0006 幻覺主因）；state 快取重用；encoder 視窗依長度裁切。
        fn full_streaming(
            &self,
            speech: &[f32],
            initial_prompt: Option<&str>,
        ) -> Result<String, RaflowError> {
            let mut state_guard = self.lock_state()?;
            let state = state_guard
                .as_mut()
                .ok_or_else(|| RaflowError::WhisperInference {
                    detail: "whisper state unavailable".into(),
                })?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some(self.language.as_str()));
            params.set_translate(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);
            params.set_print_special(false);
            // 抗幻覺：temperature 0 + 抑制 non-speech tokens。
            params.set_temperature(0.0);
            params.set_suppress_nst(true);
            // 預設不設 initial_prompt（ADR-0006 幻覺主因）；Some = 術語/命令字
            // priming（回吐由呼叫端 is_prompt_echo 守門）。
            if let Some(p) = initial_prompt {
                params.set_initial_prompt(p);
            }
            // state 跨呼叫重用（spec §16）：斷開 state 內殘留的上輪文字 context
            // （prompt_past），維持「每次呼叫獨立」——與原本每次新建 state 行為一致。
            params.set_no_context(true);
            // encoder 視窗裁到實際長度（滾動尾句 2~5s 免付 30s 滿視窗，spec §16）。
            params.set_audio_ctx(audio_ctx_for_samples(speech.len()));

            state
                .full(params, speech)
                .map_err(|e| RaflowError::WhisperInference {
                    detail: format!("state.full: {e}"),
                })?;

            let mut out = String::new();
            for segment in state.as_iter() {
                let text = segment
                    .to_str_lossy()
                    .map_err(|e| RaflowError::WhisperInference {
                        detail: format!("segment.to_str_lossy: {e}"),
                    })?;
                out.push_str(text.trim());
            }
            Ok(out)
        }

        /// Phase 2 句級滾動用（ADR-0006 §8.7.2）：用（快取的）Silero VAD 切 `pcm_i16`
        /// （16 kHz mono），回傳每個語音段的**取樣索引範圍**（呼叫端可對 `pcm_i16` 切片
        /// 串接後餵 [`transcribe_span`](Self::transcribe_span)）。空 / 靜音 → 空 `Vec`。
        pub fn speech_segments(
            &self,
            pcm_i16: &[i16],
            vad_model_path: &Path,
        ) -> Result<Vec<std::ops::Range<usize>>, RaflowError> {
            if pcm_i16.is_empty() {
                return Ok(Vec::new());
            }
            if !vad_model_path.exists() {
                return Err(RaflowError::WhisperModelMissing {
                    path: vad_model_path.to_path_buf(),
                });
            }

            let mut audio_f32 = vec![0.0_f32; pcm_i16.len()];
            convert_integer_to_float_audio(pcm_i16, &mut audio_f32).map_err(|e| {
                RaflowError::WhisperInference {
                    detail: format!("convert_integer_to_float_audio: {e}"),
                }
            })?;
            self.vad_ranges(vad_model_path, &audio_f32)
        }
    }
}

#[cfg(feature = "whisper")]
pub use imp::WhisperContext;

#[cfg(not(feature = "whisper"))]
pub use stub::WhisperContext;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_path_respects_env_override() {
        // 用 dotenv 風格的隔離測試很困難（env 是 process global）；改驗純函式邏輯：
        // default_model_path 與 HOME 對應；resolve_model_path 在 RAFLOW_WHISPER_MODEL
        // 缺席時 fallback 到 default_model_path。檔名依「turbo > small」偏好序挑選
        // （視本機 models/ 目錄實際存在的檔案而定），但目錄一定落在 raflow/models。
        if let Some(p) = default_model_path() {
            assert!(
                p.parent().is_some_and(|d| d.ends_with("raflow/models")),
                "default path should land in raflow/models, got {}",
                p.display()
            );
            assert!(
                MODEL_PREFERENCE
                    .iter()
                    .any(|name| p.file_name().is_some_and(|f| f == *name)),
                "default filename should be one of MODEL_PREFERENCE, got {}",
                p.display()
            );
        }
    }

    /// 模型挑選（turbo 換裝，spec/whisper.md §4）：turbo 存在 → 優先；否則 small；
    /// 都不存在 → 回 small 路徑（維持「檔案缺失 → 提示 + 回退 Apple-only」的舊訊息）。
    #[test]
    fn preferred_model_in_prefers_turbo_over_small() -> Result<(), std::io::Error> {
        // (目錄中存在的檔案, 期望挑中的檔名)
        type Case<'a> = (&'a [&'a str], &'a str);
        let cases: &[Case] = &[
            (&[], "ggml-small.bin"),
            (&["ggml-small.bin"], "ggml-small.bin"),
            (
                &["ggml-large-v3-turbo-q5_0.bin"],
                "ggml-large-v3-turbo-q5_0.bin",
            ),
            (
                &["ggml-small.bin", "ggml-large-v3-turbo-q5_0.bin"],
                "ggml-large-v3-turbo-q5_0.bin",
            ),
        ];
        for (i, (present, expected)) in cases.iter().enumerate() {
            let dir =
                std::env::temp_dir().join(format!("raflow-model-pick-{}-{i}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            for name in *present {
                std::fs::write(dir.join(name), b"")?;
            }
            let picked = preferred_model_in(&dir);
            assert!(
                picked.file_name().is_some_and(|f| f == *expected),
                "case {i}: expected {expected}, got {}",
                picked.display()
            );
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    /// Encoder 視窗裁切（延遲優化）：whisper.cpp 對任意長度輸入都補零到 30s 視窗
    /// （1500 audio-ctx tokens）跑滿 encoder；對 2~5s 的滾動尾句是 ~10 倍浪費。
    /// `audio_ctx_for_samples` 依樣本數換算所需 tokens（1 token = 320 samples = 20ms）
    /// 加安全餘裕，夾在 [下限, 1500]：短句省 encoder、長音訊行為與原本相同。
    #[test]
    fn audio_ctx_for_samples_scales_with_margin_and_clamps() {
        // (samples, expected_tokens)。下限 512：harness 實證 256 造成 turbo 句內
        // ×2 重複與截斷（見 AUDIO_CTX_MIN doc）。
        let cases: &[(usize, i32)] = &[
            (0, 512),           // 空 → 下限
            (16_000, 512),      // 1s → 50+64=114 → 下限 512
            (100_001, 512),     // ceil(100001/320)+64 = 377 → 仍低於下限
            (143_360, 512),     // 448+64 = 512 → 恰為下限
            (160_000, 564),     // 10s → 500+64
            (320_000, 1_064),   // 20s → 1000+64
            (480_000, 1_500),   // 30s → 1500+64 → clamp 上限
            (1_000_000, 1_500), // 超過 30s（full 內部分窗）→ 上限，行為同未設
        ];
        for (samples, expected) in cases.iter().copied() {
            assert_eq!(
                audio_ctx_for_samples(samples),
                expected,
                "audio_ctx_for_samples({samples})"
            );
        }
    }

    /// 滾動游標穩定性（實測回歸防護）：VAD 對成長中的緩衝重切段時，段的合併/
    /// 分裂會讓「段數游標」指錯音訊 → 已定稿內容被重複轉錄（實測「另外我們」重複、
    /// 尾句 ×4）。`segments_pending` 以**樣本位置** cutoff 過濾：完全在 cutoff 前的段
    /// 剔除、跨界的段裁到 cutoff 起（VAD pad 回溯屬靜音，裁掉無害）、之後的段原樣。
    #[test]
    fn segments_pending_drops_and_clips_by_sample_cutoff() {
        type Case<'a> = (&'a [(usize, usize)], usize, &'a [(usize, usize)]);
        let cases: &[Case] = &[
            // cutoff 0 → 全保留
            (&[(0, 100), (200, 300)], 0, &[(0, 100), (200, 300)]),
            // 段完全在 cutoff 前 → 剔除
            (&[(0, 100), (200, 300)], 100, &[(200, 300)]),
            (&[(0, 100), (200, 300)], 150, &[(200, 300)]),
            // 跨界段 → 裁到 cutoff 起
            (&[(0, 100), (80, 300)], 100, &[(100, 300)]),
            // cutoff 之後 → 原樣
            (&[(200, 300)], 100, &[(200, 300)]),
            // 全部在 cutoff 前 → 空
            (&[(0, 100)], 500, &[]),
            // 空輸入 → 空
            (&[], 100, &[]),
        ];
        for (i, (segs, cutoff, expected)) in cases.iter().enumerate() {
            let segs: Vec<std::ops::Range<usize>> = segs.iter().map(|&(s, e)| s..e).collect();
            let expected: Vec<std::ops::Range<usize>> =
                expected.iter().map(|&(s, e)| s..e).collect();
            assert_eq!(
                segments_pending(&segs, *cutoff),
                expected,
                "case {i}: cutoff={cutoff}"
            );
        }
    }

    /// 重複迴圈幻覺守門（實測回歸防護）：Whisper decoder 重複迴圈會吐出
    /// 「同一句 ×N」（實測「我們會在一起的一段時間 開啟動了」×4），純中文可通過
    /// `is_safe_whisper_output`。`is_repetition_loop`：同一 ≥5 字元單元**連續**出現
    /// ≥3 次 → 退化輸出，呼叫端拒收（不鎖定，留待下輪更長上下文重試）。
    #[test]
    fn is_repetition_loop_catches_degenerate_output() {
        let looped = [
            "我們會在一起的一段時間 開啟動了我們會在一起的一段時間 開啟動了我們會在一起的一段時間 開啟動了我們會在一起的一段時間 開啟動了",
            "所以說我們會在一起的一段時間 開啟動了我們會在一起的一段時間 開啟動了我們會在一起的一段時間 開啟動了",
            "這是測試這是測試這是測試",
            "abcdefabcdefabcdef",
            // turbo 實測回歸案例：單元 3 字（含空白）×4 漏網
            "觀看 觀看 觀看 觀看",
        ];
        let normal = [
            "",
            "正常的一句話",
            "我們公司的CI/CD工具會是使用ArgoCD",
            "好好好",           // 單元 <5 字，日常口語不誤殺
            "哈哈哈哈哈哈",     // 同上
            "這是測試這是測試", // 只重複 2 次，不足 3
            "點點點點很常見但單元太短不算",
        ];
        for text in looped {
            assert!(is_repetition_loop(text), "should catch: {text:?}");
        }
        for text in normal {
            assert!(!is_repetition_loop(text), "false positive: {text:?}");
        }
    }

    /// Prompt priming（離線評估驗證）：術語 + 命令字餵給滾動路徑的
    /// initial_prompt，讓 Whisper 拼出標準形（ArgoCD 而非 R5CT、逗點/換行而非 多點/萬行）。
    /// `build_rolling_prompt`：上限 30 個術語 + 內建命令字；`is_prompt_echo`：輸出含
    /// prompt 的「相鄰詞對, 格式」（如 "ArgoCD, GitLab CI"）→ 判定回吐（防範 ADR-0006 已知幻覺模式）。
    #[test]
    fn rolling_prompt_builds_capped_and_detects_echo() {
        // build：含術語與命令字；超過上限截斷。
        let few = build_rolling_prompt(&["ArgoCD", "Terraform"]);
        for kw in ["ArgoCD", "Terraform", "逗點", "句點", "換行"] {
            assert!(few.contains(kw), "prompt 應含 {kw:?}：{few:?}");
        }
        let many: Vec<String> = (0..35).map(|i| format!("Term{i}")).collect();
        let many_refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let capped = build_rolling_prompt(&many_refs);
        assert!(
            capped.contains("Term29") && !capped.contains("Term30"),
            "應截斷至 30 個術語：{capped:?}"
        );

        // echo：scaffold（詞彙：）或相鄰詞對「A、B」格式 → 回吐；正常敘述 → 非回吐。
        let terms = ["ArgoCD", "GitLab CI", "Terraform"];
        let echo_cases = [
            "ArgoCD、GitLab CI",
            "前面講到 GitLab CI、Terraform 之類的",
            "詞彙：ArgoCD",
        ];
        let normal_cases = [
            "",
            "我們用ArgoCD跟GitLab CI部署",
            "Terraform是我們的IaC工具",
            "我們公司的CI CD工具是使用GitLab CI跟ArgoCD",
        ];
        for t in echo_cases {
            assert!(is_prompt_echo(t, &terms), "應判定回吐: {t:?}");
        }
        for t in normal_cases {
            assert!(!is_prompt_echo(t, &terms), "誤判回吐: {t:?}");
        }
    }

    /// 自適應鎖定門檻（真人錄音實證）：句間停頓實測僅 1.17~1.39s、
    /// 句內思考停頓可達 1.6s——單一門檻無法分離。解法：累積語音越長門檻越低——
    /// 累積 <5s → 2.0s（防句內思考停頓提前鎖）；≥5s → 1.0s（長句後任何像樣停頓即鎖）。
    #[test]
    fn rolling_trailing_silence_scales_with_pending_speech() {
        // (pending_speech_samples, expected_threshold_samples)
        let cases: &[(usize, usize)] = &[
            // <1.5s：孤兒短片段（真人錄音實測「換行 再來」1.04s 段落長停頓後單獨
            // 鎖定 → 無上下文轉出「和一半」等退化輸出）→ 永不因靜音鎖定，等 is_final
            // 或更多語音併入下一句。
            (0, usize::MAX),
            (16_000, usize::MAX), // 1.0s → 不鎖
            (23_999, usize::MAX), // 差 1 sample 到 1.5s → 不鎖
            (24_000, 32_000),     // 恰 1.5s → 基礎門檻 2.0s
            (79_999, 32_000),     // 差 1 sample 到 5s → 仍 2.0s
            (80_000, 16_000),     // 恰 5s → 快速門檻 1.0s
            (160_000, 16_000),    // 10s → 1.0s
        ];
        for (pending, expected) in cases.iter().copied() {
            assert_eq!(
                rolling_trailing_silence_for(pending),
                expected,
                "rolling_trailing_silence_for({pending})"
            );
        }
    }

    /// Phase 2 停頓即鎖（整句一起）：只在最後一段後靜音 ≥ 門檻時，把累積的所有段一起定稿；
    /// 講話中（含句內小停頓分出的段）皆不定稿；停止時全定稿。參數化覆蓋邊界。
    /// 靜音門檻取 4800 samples（0.3s @ 16kHz）；`buffer_len - 最後段 end` 為段末靜音長度。
    #[test]
    fn segments_ready_to_finalize_locks_whole_sentence_on_pause() {
        const SIL: usize = 4_800;
        // (segment_ends, buffer_len, finalized, is_final, expected_start, expected_end)
        type Case<'a> = (&'a [usize], usize, usize, bool, usize, usize);
        let cases: &[Case] = &[
            // 無段 → 空
            (&[], 16_000, 0, false, 0, 0),
            // 唯一段、段末靜音不足（仍在講）→ 不定稿
            (&[14_000], 16_000, 0, false, 0, 0),
            // 唯一段、段末靜音足夠（6000 ≥ 4800）→ 停頓，定稿
            (&[10_000], 16_000, 0, false, 0, 1),
            // 多段、最後段仍在講（靜音不足）→ 全不定稿（關鍵：句內停頓分出的段不逐段鎖）
            (&[10_000, 20_000, 28_000], 30_000, 0, false, 0, 0),
            // 多段、最後段靜音足夠（5000 ≥ 4800）→ 累積的段**一起**定稿（整句一次）
            (&[10_000, 20_000, 25_000], 30_000, 0, false, 0, 3),
            // 已定稿 2 段、最後段靜音足夠 → 只新定稿剩下那段
            (&[10_000, 20_000, 25_000], 30_000, 2, false, 2, 3),
            // 停止收尾：不看靜音，全定稿
            (&[10_000, 28_000], 30_000, 0, true, 0, 2),
            (&[10_000], 16_000, 0, true, 0, 1),
            // 防禦：finalized > 可定稿數 → 空範圍，不 panic、不越界
            (&[10_000], 16_000, 5, false, 1, 1),
        ];
        for (ends, buf, finalized, is_final, exp_start, exp_end) in cases.iter().copied() {
            let r = segments_ready_to_finalize(ends, buf, finalized, SIL, is_final);
            assert_eq!(
                (r.start, r.end),
                (exp_start, exp_end),
                "segments_ready_to_finalize({ends:?}, {buf}, {finalized}, {SIL}, {is_final})"
            );
            assert!(r.start <= r.end && r.end <= ends.len(), "範圍越界: {r:?}");
        }
    }

    /// 收尾 flush 的成敗判定：有鎖定句、或本來就無 pending 語音 → 已交付
    /// （可安全送空 Final 觸發剪貼簿）；否則呼叫端必須放行 Apple final 回退——
    /// 空 Final 會讓 printer 把未定稿草稿整段 backspace 清掉（資料遺失路徑：
    /// 守門拒收 / Whisper 空輸出 / VAD 失敗時觸發）。
    #[test]
    fn rolling_final_flush_delivered_requires_lock_or_no_pending() {
        let cases: &[(bool, usize, bool)] = &[
            (true, 0, true),      // 有鎖定、無殘留 → 交付
            (true, 16_000, true), // 有鎖定（is_final range 涵蓋全部 pending）→ 交付
            (false, 0, true),     // 無鎖定但本來就無語音（靜音收尾）→ 交付（空 Final 無害）
            (false, 1, false),    // 有 pending 語音卻沒鎖定（拒收/空輸出）→ 未交付，須回退
            (false, 48_000, false),
        ];
        for (locked, pending, expected) in cases.iter().copied() {
            assert_eq!(
                rolling_final_flush_delivered(locked, pending),
                expected,
                "rolling_final_flush_delivered({locked}, {pending})"
            );
        }
    }

    /// `RAFLOW_ROLLING` 純解析：預設 ON；只有明確的 off 值（空/0/false）才 OFF，
    /// 其餘（含未設）ON。
    #[test]
    fn parse_rolling_flag_defaults_on_and_recognizes_off_values() {
        let cases: &[(Option<&str>, bool)] = &[
            (None, true),         // 未設 → ON（預設；opt-out 用 RAFLOW_ROLLING=0）
            (Some(""), false),    // 空字串 → OFF
            (Some("   "), false), // 純空白 → OFF
            (Some("0"), false),   // "0" → OFF
            (Some("false"), false),
            (Some("False"), false),
            (Some("FALSE"), false),
            (Some("1"), true), // 任何非 off 值 → ON
            (Some("true"), true),
            (Some("on"), true),
            (Some("yes"), true),
            (Some(" 1 "), true),       // 前後空白修剪後仍 ON
            (Some("false-ish"), true), // 非精確 "false" → ON（保守：只認精確 off 值）
        ];
        for (input, expected) in cases.iter().copied() {
            assert_eq!(
                parse_rolling_flag(input),
                expected,
                "parse_rolling_flag({input:?})"
            );
        }
    }

    /// VAD centisecond → sample 範圍：×160、clamp 到 [0,total]、start>end 收斂為空且不越界。
    #[test]
    fn vad_cs_to_samples_scales_and_clamps() {
        // (start_cs, end_cs, total, exp_start, exp_end)
        let cases: &[(f32, f32, usize, usize, usize)] = &[
            (0.0, 10.0, 16_000, 0, 1_600),          // 0~100ms → 0..1600
            (5.0, 15.0, 16_000, 800, 2_400),        // 一般段
            (0.0, 200.0, 16_000, 0, 16_000),        // end 超過 total → clamp 到 total
            (-1.0, 10.0, 16_000, 0, 1_600),         // 負 start → clamp 到 0
            (10.0, 10.0, 16_000, 1_600, 1_600),     // 零長度段 → 空
            (5.0, 3.0, 16_000, 480, 480),           // start>end（VAD 異常）→ 收斂為空、不越界
            (300.0, 400.0, 16_000, 16_000, 16_000), // 全超界 → total..total 空
        ];
        for (s, e, total, exp_s, exp_e) in cases.iter().copied() {
            let r = vad_cs_to_samples(s, e, total);
            assert_eq!(
                (r.start, r.end),
                (exp_s, exp_e),
                "vad_cs_to_samples({s},{e},{total})"
            );
            assert!(r.start <= r.end && r.end <= total, "越界: {r:?}");
        }
    }

    /// ADR-0006 §2.2 cumulative 前綴裁切：以 char 為單位剝掉已鎖定前綴；過長 → 空字串。
    #[test]
    fn strip_committed_prefix_trims_locked_chars() {
        let cases: &[(&str, usize, &str)] = &[
            ("你好世界", 0, "你好世界"), // 尚無鎖定 → 原樣
            ("你好世界", 2, "世界"),     // 剝掉前 2 個中文字
            ("你好世界", 4, ""),         // 全部鎖定 → 空
            ("你好世界", 10, ""),        // 過長（Apple 回改變短）→ 空、不 panic
            ("hello world", 6, "world"), // ASCII 以 char 計
            ("", 3, ""),                 // 空輸入
            ("a你b", 1, "你b"),          // 混排以 char（非 byte）計
        ];
        for (cumulative, committed, expected) in cases.iter().copied() {
            assert_eq!(
                strip_committed_prefix(cumulative, committed),
                expected,
                "strip_committed_prefix({cumulative:?}, {committed})"
            );
        }
    }

    /// 確認 S2TWP 產生**台灣**繁體 + 詞彙轉換（軟體 而非 軟件；程式 而非 程序），這是句級
    /// 滾動路徑補繁體的依據。缺 whisper feature 時 OpenCC 不編入，跳過。
    #[cfg(feature = "whisper")]
    #[test]
    fn opencc_s2twp_converts_to_taiwan_traditional() {
        use ferrous_opencc::OpenCC;
        use ferrous_opencc::config::BuiltinConfig;
        let cc = match OpenCC::from_config(BuiltinConfig::S2twp) {
            Ok(cc) => cc,
            Err(e) => panic!("OpenCC S2TWP 初始化失敗: {e}"),
        };
        let cases = [
            ("测试一下中文", "測試一下中文"),
            ("软件", "軟體"), // 台灣詞彙（非 軟件）
            ("程序", "程式"), // 台灣詞彙（非 程序/程序）
            ("请使用 Cursor", "請使用 Cursor"),
        ];
        for (s, t) in cases {
            assert_eq!(cc.convert(s), t, "S2TWP({s:?})");
        }
    }

    #[test]
    fn default_vad_model_path_lands_in_models_dir() {
        // Phase 1：VAD model 路徑與 whisper model 同目錄，檔名沿用上游官方發佈名。
        if let Some(p) = default_vad_model_path() {
            assert!(
                p.ends_with("raflow/models/ggml-silero-v6.2.0.bin"),
                "default VAD path should be raflow/models/ggml-silero-v6.2.0.bin, got {}",
                p.display()
            );
        }
    }

    #[test]
    fn load_returns_model_missing_for_nonexistent_path() {
        let bogus = Path::new("/tmp/raflow-test-definitely-missing-model.bin");
        let result = WhisperContext::load(bogus, "zh");
        match result {
            Err(RaflowError::WhisperModelMissing { path }) => {
                assert_eq!(path, bogus.to_path_buf());
            }
            // 若編譯時無 feature，stub 走 WhisperLoad 路徑亦合法（兩種都明確報「無法用」）
            Err(RaflowError::WhisperLoad { .. }) => {}
            Err(other) => panic!("expected WhisperModelMissing or WhisperLoad, got {other:?}"),
            Ok(_) => panic!("must not load a non-existent model"),
        }
    }

    #[cfg(feature = "whisper")]
    #[test]
    fn transcribe_empty_returns_empty_string() {
        // empty PCM 不該觸發任何 inference 路徑，直接回 ""
        let bogus = Path::new("/tmp/raflow-test-definitely-missing-model.bin");
        // load 一定失敗（model 不存在），故無法直接構造 ctx；本 test 只驗 stub 路徑下的行為
        let _ = WhisperContext::load(bogus, "zh");
        // 真正的 transcribe 行為由 #[ignore] 整合測試覆蓋（需要真 model）
    }

    /// Whisper dictation 規範化：Phase 5-fix5。
    /// 兩階段：standalone 替換 → collapse 鄰接標點（含 \n 吸收）。
    #[test]
    fn normalize_dictation_commands_handles_all_patterns() {
        let cases: &[(&str, &str)] = &[
            // === Standalone 替換 ===
            ("逗點", "，"),
            ("句點", "。"),
            ("問號", "？"),
            ("驚嘆號", "！"),
            ("換行", "\n"),
            // === 使用者實機 case：image #4 ===
            ("好，讓我們繼續下去,逗點", "好，讓我們繼續下去，"),
            // === 半全形混合 + 標點 + 變體 ===
            ("你好,逗點", "你好，"),
            ("結束.句號", "結束。"),
            ("為什麼?問號", "為什麼？"),
            ("快點!驚嘆號", "快點！"),
            // === 三明治結構 ===
            ("a，逗點，b", "a，b"),
            ("這裡，鬥點，那裡", "這裡，那裡"),
            // === 同音字 hallucination 變體 ===
            ("命令是鬥點", "命令是，"),
            ("命令是聚點", "命令是。"),
            ("段落緩行下一段", "段落\n下一段"),
            ("逗典", "，"),
            ("兜點", "，"),
            ("荒航", "\n"),
            // === 實機回歸案例：說「逗點 換行」——
            // 命令產生的逗點必須保留（不可被 \n 吸收），詞間空白要吸收 ===
            ("是我們的CI，逗典 荒航", "是我們的CI，\n"),
            ("逗點換行", "，\n"),
            ("逗點 換行", "，\n"),
            // === 換行：後側「Whisper 原生」標點被 \n 吸收；前側標點只在「緊跟另一個
            // 命令 / 位於開頭」時才吸（Whisper 會把使用者說的「逗點」直接轉成「，」，
            // 與刻意 dictate 無法區分 → 字後的標點一律保留）===
            ("第一段，換行，第二段", "第一段，\n第二段"),
            ("第一段換行第二段", "第一段\n第二段"),
            ("第一段 換行 第二段", "第一段\n第二段"),
            ("。換行。", "\n"),
            // === 實機回歸案例：Whisper 把「逗點」轉成
            // 「，」+「換行」聽成「換航」；空白 + 保留逗點 + 換行 ===
            ("Teraphone ， 換航", "Teraphone，\n"),
            ("11CI換航另外", "11CI\n另外"),
            // === 使用者連 dictation case ===
            ("「鬥點，聚點，換行。」", "「，。\n」"),
            // === 連續多種命令 ===
            ("第一句,逗點第二句.句點", "第一句，第二句。"),
            // === Cross 半全形 collapse 直接驗 ===
            ("a,，b", "a，b"),
            ("a，,b", "a，b"),
            ("a,,,b", "a，b"),
            // === FP risk（aggressive 替換已知 trade-off，spec §13.2）===
            // ("我有一個逗點的疑問", "我有一個逗點的疑問"),  // 會被誤替換
            // === Edge cases ===
            ("", ""),
            ("   ", "   "),
            ("純文字無命令字", "純文字無命令字"),
        ];
        for (input, expected) in cases {
            let actual = normalize_dictation_commands(input);
            assert_eq!(
                actual, *expected,
                "normalize({input:?}) → {actual:?} ≠ {expected:?}"
            );
        }
    }

    /// Safety filter 邊界覆蓋：通過 = 純中英數標點；阻擋 = 含韓文 / 日文假名 / 西里爾 等。
    /// 阻擋情境會讓呼叫端 fallback 到 Apple final，避免「Whisper 改一改變更糟」。
    #[test]
    fn is_safe_whisper_output_allows_chinese_english_only() {
        let allow = [
            "",
            "Hello world.",
            "我打開 Cursor 跑 npm install",
            "繁體中文逐字稿。",
            "句號、頓號、引號「」都該允許",
            "ABC 123 你好 .,!?",
            "全形　空白都OK",
        ];
        let block = [
            // 韓文（使用者反映過）
            "안녕하세요 你好",
            "我打開 안녕",
            // 日文假名
            "こんにちは",
            "我說了 さようなら",
            "カタカナ",
            // 西里爾
            "Привет",
            // 阿拉伯
            "مرحبا",
        ];
        for s in allow {
            assert!(is_safe_whisper_output(s), "should ALLOW: {s:?}");
        }
        for s in block {
            assert!(!is_safe_whisper_output(s), "should BLOCK: {s:?}");
        }
    }
}
