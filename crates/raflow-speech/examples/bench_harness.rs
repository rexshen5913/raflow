//! Whisper 效能／準確度 **benchmark harness**——對固定 TTS/錄音素材量測本地 Whisper 終校的
//! 轉錄延遲、即時率（RTF），並用**已知逐字稿**算字元錯誤率（CER），支援多模型比較。
//!
//! 動機：門檻/模型調校過去靠「體感」；本 harness 用可重現素材把「Whisper 轉錄要多久、準不準」
//! 變成數據，取代主觀判斷（對應 review 建議 #3）。
//!
//! 用法：
//!   make whisper-model-turbo tts-fixtures     # 一次性準備模型與素材
//!   cargo run -p raflow-speech --example bench_harness --features whisper --release
//!   RAFLOW_BENCH_MODELS=/path/a.bin,/path/b.bin cargo run ... --example bench_harness ...
//!                                             # 比較多個模型（turbo vs small）
//!   cargo test --example bench_harness         # 只跑 CER 單元測試（免模型）
//!
//! **量測範圍（誠實界定）**：
//! - ✅ Whisper 終校延遲（`transcribe` 全段）、即時率 RTF=延遲/音訊長度、**內容字元 CER**。
//!   CER 正規化時**去標點/空白、ASCII 小寫**（見 [`normalize`]），只比字母數字與 CJK → 它量的是
//!   **內容辨識準確度**（抓錯字/漏字/幻覺，如 t5「得/的」、mixed_long「Gillab」）。
//! - ❌ **不涵蓋 dictation 命令轉換正確性**：`transcribe` 雖會把「逗點→，、換行→\n」，但那些**標點
//!   正好被 CER 正規化掉** → 本指標**看不到**命令有沒有轉對（app 若整個漏掉逗點，CER 一樣是 0）。
//!   要驗 dictation 轉換需另做針對性斷言（本 harness 未做）。對照基準用**預期最終輸出**（命令字已
//!   移除/轉換），純粹是為了讓「內容」對得齊、不把命令字面當內容誤判。
//! - ❌ **Apple draft latency**：屬 live `SFSpeechRecognizer` 串流，離線 harness 量不到——需在 app
//!   內對 partial callback 打點（另案）。
//! - ⚠️ **CPU/GPU 佔用**：延遲為 wall-clock（Whisper.cpp 走 Metal GPU）；精確 GPU/CPU 佔用需
//!   Instruments / `powermetrics` 外部工具，非本純 Rust harness 範圍。RTF 為可行動的效能代理值。

use std::path::{Path, PathBuf};
use std::time::Instant;

use raflow_core::RaflowError;
use raflow_speech::{WhisperContext, resolve_model_path};

const SAMPLE_RATE: usize = 16_000;

/// 一個 benchmark 素材：檔案路徑（相對 raflow/）＋可選的**預期最終輸出**（`None` = 只量延遲不算
/// CER）。`truth` 是「`transcribe` 正規化後應得到的字串」，不是生說話文字——對含 dictation 命令的
/// 素材（如 t6），生說話的「逗點/換行」在此已寫成轉換後的「，/換行字元」，否則 CER 會把正確轉換誤判為錯。
struct Fixture {
    file: &'static str,
    truth: Option<&'static str>,
}

/// 素材與**預期最終輸出**。t1~t5 無命令字 → 生文本即預期輸出（取自 `make tts-fixtures` 的 `say`
/// 輸入）；t6 的說話文字為「今天天氣很好 逗點 換行 明天會更好」，`transcribe` 會把「逗點→，、
/// 換行→\n」，故 truth 寫成轉換後的形式。
const FIXTURES: &[Fixture] = &[
    Fixture { file: "testdata/tts/t1.wav", truth: Some("我們公司的 CI CD 工具是 ArgoCD") },
    Fixture { file: "testdata/tts/t2.wav", truth: Some("然後我們會使用 Terraform 來管理基礎設施") },
    Fixture { file: "testdata/tts/t3.wav", truth: Some("第一句話講完了") },
    Fixture { file: "testdata/tts/t4.wav", truth: Some("接下來是第二句話") },
    Fixture { file: "testdata/tts/t5.wav", truth: Some("這是一段連續說話中間完全不停頓的比較長的句子") },
    // 說話：「今天天氣很好 逗點 換行 明天會更好」→ 命令字轉換後的預期輸出。注意 normalize() 會把
    // 「，/換行」去掉，故 t6 的 CER **只反映內容**（如是否幻覺多字），**不驗**逗點/換行有沒有轉對。
    Fixture { file: "testdata/tts/t6.wav", truth: Some("今天天氣很好，\n明天會更好") },
    // 真人中英混合長句：預期輸出未知 → 只量延遲/RTF，不算 CER。
    Fixture { file: "testdata/mixed_long.wav", truth: None },
];

/// 正規化：只留字母數字與 CJK、ASCII 轉小寫、去空白與標點——中英混合逐字比較的共同基準。
/// `char::is_alphanumeric()` 對 CJK 表意字回傳 true、對空白/標點回傳 false。
/// **後果**：標點被丟掉 → 由此算出的 CER 對標點/dictation 命令轉換**不敏感**，量的是內容而非標點正確性。
fn normalize(s: &str) -> Vec<char> {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 兩序列的 Levenshtein 編輯距離（rolling 兩列 DP，O(n·m) 時間、O(m) 空間）。
fn levenshtein(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// 內容字元錯誤率（%）：`Levenshtein(ref, hyp) / len(ref)`，[`normalize`] 後比較（**標點/空白不計**）。
/// ref 為空時：hyp 也空 → 0；否則 → 100（無從歸一，記為全錯）。
fn cer(reference: &str, hypothesis: &str) -> f32 {
    let r = normalize(reference);
    let h = normalize(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 100.0 };
    }
    levenshtein(&r, &h) as f32 / r.len() as f32 * 100.0
}

/// 單一素材的量測結果。
struct Row {
    file: &'static str,
    audio_ms: f32,
    whisper_ms: f32,
    rtf: f32,
    cer: Option<f32>,
    text: String,
}

fn bench_model(model: &Path) -> Result<(), RaflowError> {
    println!("\n=== model: {} ===", model.display());
    let whisper = WhisperContext::load(model, "zh")?;

    // 先蒐集 PCM（跳過不存在的素材），並以第一個素材做一次**未計時暖機**——首次推論含 Metal
    // shader 編譯／模型載入，會灌水延遲；暖機後每筆計時才可比。
    let mut loaded: Vec<(&Fixture, Vec<i16>)> = Vec::new();
    for fx in FIXTURES {
        let path = Path::new(fx.file);
        if !path.exists() {
            println!("  skip {}（不存在——make tts-fixtures？）", fx.file);
            continue;
        }
        loaded.push((fx, read_wav_i16_16k_mono(path)?));
    }
    if let Some((_, pcm)) = loaded.first() {
        let _ = whisper.transcribe(pcm)?; // 暖機（不計時）
    }

    let mut rows: Vec<Row> = Vec::new();
    for (fx, pcm) in &loaded {
        let audio_ms = pcm.len() as f32 / (SAMPLE_RATE as f32 / 1000.0);
        let t0 = Instant::now();
        let text = whisper.transcribe(pcm)?;
        let whisper_ms = t0.elapsed().as_secs_f32() * 1000.0;
        rows.push(Row {
            file: fx.file,
            audio_ms,
            whisper_ms,
            rtf: whisper_ms / audio_ms,
            cer: fx.truth.map(|t| cer(t, &text)),
            text,
        });
    }

    println!(
        "\n  {:<26} {:>9} {:>10} {:>6} {:>7}   輸出",
        "fixture", "audio_ms", "whisper_ms", "RTF", "CER%"
    );
    for r in &rows {
        let cer_s = r.cer.map_or_else(|| "  —  ".to_string(), |c| format!("{c:5.1}"));
        let name = r.file.rsplit('/').next().unwrap_or(r.file);
        println!(
            "  {:<26} {:>9.0} {:>10.0} {:>6.2} {:>7}   {}",
            name, r.audio_ms, r.whisper_ms, r.rtf, cer_s, r.text.trim()
        );
    }

    // 彙總：平均 RTF、平均 CER（僅計有 ground truth 者）。
    if !rows.is_empty() {
        let mean_rtf = rows.iter().map(|r| r.rtf).sum::<f32>() / rows.len() as f32;
        let cers: Vec<f32> = rows.iter().filter_map(|r| r.cer).collect();
        let mean_cer = if cers.is_empty() {
            "—".to_string()
        } else {
            format!("{:.1}%", cers.iter().sum::<f32>() / cers.len() as f32)
        };
        println!(
            "\n  彙總：平均 RTF {mean_rtf:.2}（<1 = 快於即時）、平均內容 CER {mean_cer}（忽略標點、\
             不含 dictation 轉換正確性；{} 筆有逐字稿）",
            cers.len()
        );
    }
    Ok(())
}

fn main() -> Result<(), RaflowError> {
    println!("raflow Whisper benchmark（延遲=wall-clock/Metal；CER=內容準確度、不含 dictation 轉換/標點；Apple draft latency 與精確 GPU 佔用不在範圍）");
    let models: Vec<PathBuf> = match std::env::var("RAFLOW_BENCH_MODELS") {
        Ok(v) if !v.trim().is_empty() => {
            v.split(',').map(|s| PathBuf::from(s.trim())).collect()
        }
        _ => vec![resolve_model_path().ok_or_else(|| RaflowError::WhisperLoad {
            detail: "找不到 whisper model（make whisper-model-turbo，或設 RAFLOW_BENCH_MODELS）".into(),
        })?],
    };
    for model in &models {
        bench_model(model)?;
    }
    Ok(())
}

/// 極簡 WAV 讀取：只接受 16 kHz / mono / 16-bit PCM（與 rolling_harness / vad_harness 相同實作）。
fn read_wav_i16_16k_mono(path: &Path) -> Result<Vec<i16>, RaflowError> {
    let bytes = std::fs::read(path).map_err(|e| RaflowError::WhisperLoad {
        detail: format!("讀取 WAV 失敗 {}: {e}（先跑 make tts-fixtures？）", path.display()),
    })?;
    let err = |msg: String| RaflowError::WhisperLoad { detail: msg };

    let u32le = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let u16le = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(err(format!("{} 不是 RIFF/WAVE", path.display())));
    }

    let mut pos = 12usize;
    let mut fmt_ok = false;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32le(&bytes[pos + 4..pos + 8]) as usize;
        let body_start = pos + 8;
        let body_end = body_start
            .checked_add(size)
            .ok_or_else(|| err(format!("chunk size 溢位 @ offset {pos}")))?;
        if body_end > bytes.len() {
            return Err(err(format!("chunk `{}` 超出檔案長度", String::from_utf8_lossy(id))));
        }
        if id == b"fmt " {
            if size < 16 {
                return Err(err("fmt chunk 過短".into()));
            }
            let audio_format = u16le(&bytes[body_start..body_start + 2]);
            let channels = u16le(&bytes[body_start + 2..body_start + 4]);
            let rate = u32le(&bytes[body_start + 4..body_start + 8]);
            let bits = u16le(&bytes[body_start + 14..body_start + 16]);
            if audio_format != 1 && audio_format != 0xFFFE {
                return Err(err(format!("非 PCM（audioFormat={audio_format}）")));
            }
            if channels != 1 {
                return Err(err(format!("需 mono，實為 {channels} 聲道")));
            }
            if rate != 16_000 {
                return Err(err(format!("需 16 kHz，實為 {rate} Hz")));
            }
            if bits != 16 {
                return Err(err(format!("需 16-bit，實為 {bits}-bit")));
            }
            fmt_ok = true;
        } else if id == b"data" {
            data = Some(&bytes[body_start..body_end]);
        }
        pos = body_end + (size & 1);
    }

    if !fmt_ok {
        return Err(err("找不到有效 fmt chunk".into()));
    }
    let data = data.ok_or_else(|| err("找不到 data chunk".into()))?;
    let mut pcm = Vec::with_capacity(data.len() / 2);
    for frame in data.chunks_exact(2) {
        pcm.push(i16::from_le_bytes([frame[0], frame[1]]));
    }
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CER 正規化 + Levenshtein 的行為（參數化）：大小寫/空白/標點正規化後逐字比對。
    #[test]
    fn cer_normalizes_and_scores() {
        // (reference, hypothesis, expected_cer_percent)
        let cases: &[(&str, &str, f32)] = &[
            ("abc", "abc", 0.0),                        // 完全相同
            ("abcd", "abxd", 25.0),                     // 1 代換 / 4
            ("ArgoCD", "argo cd", 0.0),                 // 大小寫 + 空白正規化 → argocd == argocd
            ("我們 CI CD", "我們CICD", 0.0),             // 空白正規化後相同
            ("第一句話講完了", "第一句話講完", 100.0 / 7.0), // 少 1 字 / 7
            ("", "", 0.0),                              // 皆空
            ("", "x", 100.0),                           // ref 空、hyp 非空
            ("三個字", "", 100.0),                      // 全刪 3/3
        ];
        for (r, h, want) in cases.iter().copied() {
            let got = cer(r, h);
            assert!((got - want).abs() < 0.05, "cer({r:?}, {h:?}) = {got}, want {want}");
        }
    }
}
