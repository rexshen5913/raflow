//! Phase 2 句級滾動**離線模擬 harness**——不用真人說話，重放 TTS/錄音 WAV 逐 tick
//! 餵給與 app 完全相同的決策核心 [`rolling_tick_core`]，自動驗證滾動行為。
//!
//! 動機：實機測試（真人說話 → 看輸出）一輪要數分鐘且不可重現；本 harness 用
//! `make tts-fixtures`（macOS `say` -v Meijia，zh-TW）合成固定素材，場景可重複、
//! 可斷言、可量測延遲。核心與 `AppleSpeechBackend::rolling_tick` 共用同一份
//! `rolling_tick_core` → 離線驗證即等於驗證 app 行為（Apple partial 流除外）。
//!
//! 用法：
//!   make whisper-model-turbo whisper-vad-model tts-fixtures   # 一次性準備
//!   cargo run -p raflow-speech --example rolling_harness --features whisper --release
//!   （可選引數：fixtures 目錄，預設 `testdata/tts`；或單一 `.wav` 檔 → 只重放該檔，
//!    印出逐句鎖定與串接結果，不做關鍵詞斷言——供真人錄音驗收比對）
//!
//! 驗證面向：句數、關鍵詞、無重複注入、dictation 命令（逗點/換行）、靜音零輸出、
//! 游標單調遞增、逐 tick 延遲（含使用者最有感的「停止收尾」延遲）。

use std::path::{Path, PathBuf};
use std::time::Instant;

use raflow_core::RaflowError;
use raflow_speech::{
    WhisperContext, resolve_model_path, resolve_vad_model_path, rolling_tick_core,
};

/// Prompt priming 實驗用術語表：`RAFLOW_HARNESS_PROMPT=1` 時傳入 core。
/// 對應使用者領域（SRE/DevOps）＋本輪真人錄音的目標詞。
const EXPERIMENT_TERMS: &[&str] = &[
    "ArgoCD",
    "GitLab CI",
    "Terraform",
    "Vault",
    "CI/CD",
    "IaC",
    "Secret Manager",
    "Kubernetes",
    "Docker",
];

const SAMPLE_RATE: usize = 16_000;
/// 模擬 app 的 tick 週期（main.rs `ROLLING_TICK_INTERVAL` = 1s → 每 tick 進 16k samples）。
const SAMPLES_PER_TICK: usize = SAMPLE_RATE;

/// 場景步驟：一段 WAV 素材或一段靜音。
enum Step {
    Wav(&'static str),
    SilenceMs(usize),
}

/// 一個可斷言的滾動場景。
struct Scenario {
    name: &'static str,
    steps: &'static [Step],
    /// 期望鎖定的句數（含端點）。
    expect_phrases: (usize, usize),
    /// 每個關鍵詞必須出現在「串接後的輸出」中。
    expect_keywords: &'static [&'static str],
    /// 這些字串不得出現（如未被正規化的命令字面）。
    forbid: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "S1 單句＋停頓即鎖",
        steps: &[Step::Wav("t3.wav"), Step::SilenceMs(3_000)],
        expect_phrases: (1, 1),
        expect_keywords: &["第一句"],
        forbid: &[],
    },
    Scenario {
        name: "S2 兩句（3s 停頓分隔）",
        steps: &[
            Step::Wav("t3.wav"),
            Step::SilenceMs(3_000),
            Step::Wav("t4.wav"),
            Step::SilenceMs(3_000),
        ],
        expect_phrases: (2, 2),
        expect_keywords: &["第一句", "第二句"],
        forbid: &[],
    },
    Scenario {
        name: "S3 dictation 命令（逗點/換行）",
        steps: &[Step::Wav("t6.wav"), Step::SilenceMs(3_000)],
        expect_phrases: (1, 1),
        expect_keywords: &["，", "\n", "天氣", "明天"],
        forbid: &["逗點", "換行"],
    },
    Scenario {
        name: "S4 連續長句不切碎",
        steps: &[Step::Wav("t5.wav"), Step::SilenceMs(3_000)],
        expect_phrases: (1, 1),
        expect_keywords: &["連續"],
        forbid: &[],
    },
    Scenario {
        name: "S5 純靜音零輸出",
        steps: &[Step::SilenceMs(5_000)],
        expect_phrases: (0, 0),
        expect_keywords: &[],
        forbid: &[],
    },
    Scenario {
        name: "S6 術語雙句（術語拼法僅回報不斷言，決策 c）",
        steps: &[
            Step::Wav("t1.wav"),
            Step::SilenceMs(3_000),
            Step::Wav("t2.wav"),
            Step::SilenceMs(3_000),
        ],
        expect_phrases: (2, 2),
        // 「管理/基礎」在 t2 後半——實測曾因 audio_ctx 過小被截斷，斷言鎖住不回歸。
        expect_keywords: &["工具", "管理"],
        forbid: &[],
    },
];

/// 單句**內部**重複偵測（實測案例：「接下來是第二句話」×2 在一句內，×2 不到
/// `is_repetition_loop` 的 ×3 門檻、句間比對也抓不到）：前半 == 後半即為退化。
fn is_internally_doubled(text: &str) -> bool {
    let chars: Vec<char> = text.trim().chars().collect();
    let n = chars.len();
    n >= 8 && n % 2 == 0 && chars[..n / 2] == chars[n / 2..]
}

fn main() -> Result<(), RaflowError> {
    let arg1 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testdata/tts".to_string());
    let fixtures = PathBuf::from(&arg1);
    let model_path = resolve_model_path().ok_or_else(|| RaflowError::WhisperLoad {
        detail: "無法解析 whisper model 路徑（HOME 未設？）".into(),
    })?;
    let vad_path = resolve_vad_model_path().ok_or_else(|| RaflowError::WhisperLoad {
        detail: "無法解析 VAD model 路徑（HOME 未設？）".into(),
    })?;
    println!("== raflow 句級滾動離線 harness ==");
    println!("whisper : {}", model_path.display());
    println!("vad     : {}", vad_path.display());
    println!("fixtures: {}", fixtures.display());
    let ctx = WhisperContext::load(&model_path, "zh")?;
    // 熱身（比照 app 啟動 warm-up）：讓 Metal buffers / state 就緒，量測才反映穩態。
    let _ = ctx.transcribe(&vec![0_i16; SAMPLE_RATE]);
    println!("context 載入 + 熱身完成。\n");

    // 單檔模式：重放真人錄音，印逐句鎖定 + 串接結果（退化類斷言仍生效）。
    if arg1.ends_with(".wav") {
        let sc = Scenario {
            name: "ad-hoc WAV 重放",
            steps: &[],
            expect_phrases: (0, usize::MAX),
            expect_keywords: &[],
            forbid: &[],
        };
        let pcm = read_wav_i16_16k_mono(&fixtures)?;
        // 診斷：整段 VAD 段落與間距（判讀「停頓即鎖」門檻與實際說話節奏的關係）。
        let segs = ctx.speech_segments(&pcm, &vad_path)?;
        println!("VAD 段落（{} 段）：", segs.len());
        for (i, s) in segs.iter().enumerate() {
            let gap = if i + 1 < segs.len() {
                format!(
                    "→ 間距 {:.2}s",
                    (segs[i + 1].start - s.end) as f32 / 16_000.0
                )
            } else {
                format!(
                    "→ 檔尾靜音 {:.2}s",
                    (pcm.len().saturating_sub(s.end)) as f32 / 16_000.0
                )
            };
            println!(
                "  #{i}: {:.2}s ~ {:.2}s ({:.2}s) {gap}",
                s.start as f32 / 16_000.0,
                s.end as f32 / 16_000.0,
                (s.end - s.start) as f32 / 16_000.0
            );
        }
        println!();
        let pass = replay(&ctx, &vad_path, &pcm, &sc)?;
        println!("== 總結：{} ==", if pass { "PASS" } else { "FAIL" });
        if !pass {
            std::process::exit(1);
        }
        return Ok(());
    }

    let mut all_pass = true;
    for sc in SCENARIOS {
        all_pass &= run_scenario(&ctx, &vad_path, &fixtures, sc)?;
    }
    println!("== 總結：{} ==", if all_pass { "PASS" } else { "FAIL" });
    if !all_pass {
        std::process::exit(1);
    }
    Ok(())
}

/// 組合場景素材後重放。
fn run_scenario(
    ctx: &WhisperContext,
    vad_path: &Path,
    fixtures: &Path,
    sc: &Scenario,
) -> Result<bool, RaflowError> {
    let pcm = compose(fixtures, sc.steps)?;
    replay(ctx, vad_path, &pcm, sc)
}

/// 逐 tick 重放一段 PCM，回傳是否通過全部斷言。
/// `RAFLOW_HARNESS_DEBUG=1` 時印每 tick 的 pending 段/語音量/段末靜音（診斷鎖定時機）。
fn replay(
    ctx: &WhisperContext,
    vad_path: &Path,
    pcm: &[i16],
    sc: &Scenario,
) -> Result<bool, RaflowError> {
    println!("--- {} ---", sc.name);
    let debug = std::env::var_os("RAFLOW_HARNESS_DEBUG").is_some();
    // Prompt priming 實驗開關：RAFLOW_HARNESS_PROMPT=1 → 術語+命令字。
    let prompt_terms: Option<&[&str]> = if std::env::var_os("RAFLOW_HARNESS_PROMPT").is_some() {
        Some(EXPERIMENT_TERMS)
    } else {
        None
    };
    if prompt_terms.is_some() {
        println!(
            "  (prompt priming ON: {} 術語 + 命令字)",
            EXPERIMENT_TERMS.len()
        );
    }
    let total_secs = pcm.len() as f32 / SAMPLE_RATE as f32;

    let mut finalized = 0usize;
    let mut phrases: Vec<(usize, String)> = Vec::new(); // (tick, text)
    let mut rejected: Vec<String> = Vec::new();
    let mut max_tick_ms = 0.0f32;
    let mut cursor_ok = true;

    let mut fed = 0usize;
    let mut tick = 0usize;
    while fed < pcm.len() {
        fed = (fed + SAMPLES_PER_TICK).min(pcm.len());
        tick += 1;
        if debug {
            // 用與 core 相同的切片重現其視角（診斷 sliced-VAD 與整段 VAD 的差異）。
            let segs = ctx.speech_segments(&pcm[finalized..fed], vad_path)?;
            let speech: usize = segs.iter().map(|r| r.end - r.start).sum();
            let trailing = segs
                .last()
                .map(|s| (fed - finalized).saturating_sub(s.end))
                .unwrap_or(0);
            println!(
                "  [tick {tick:>2}] pending段={} 語音={:.2}s 段末靜音={:.2}s",
                segs.len(),
                speech as f32 / 16_000.0,
                trailing as f32 / 16_000.0
            );
        }
        let started = Instant::now();
        let out = rolling_tick_core(ctx, vad_path, &pcm[..fed], finalized, false, prompt_terms)?;
        let ms = started.elapsed().as_secs_f32() * 1_000.0;
        max_tick_ms = max_tick_ms.max(ms);
        if out.finalized_samples < finalized {
            cursor_ok = false;
        }
        finalized = out.finalized_samples;
        if let Some(p) = out.phrase {
            println!("  [tick {tick:>2} | {ms:>7.1}ms] 鎖定: {p:?}");
            phrases.push((tick, p));
        }
        rejected.extend(out.rejected);
    }
    // 停止收尾（使用者最有感的延遲）。
    let started = Instant::now();
    let out = rolling_tick_core(ctx, vad_path, pcm, finalized, true, prompt_terms)?;
    let final_ms = started.elapsed().as_secs_f32() * 1_000.0;
    if out.finalized_samples < finalized {
        cursor_ok = false;
    }
    if let Some(p) = out.phrase {
        println!("  [final   | {final_ms:>7.1}ms] 鎖定: {p:?}");
        phrases.push((tick + 1, p));
    }
    rejected.extend(out.rejected);

    for r in &rejected {
        println!("  (守門拒收: {r:?})");
    }

    // ---- 斷言 ----
    let joined: String = phrases
        .iter()
        .map(|(_, p)| p.as_str())
        .collect::<Vec<_>>()
        .join("");
    let mut failures: Vec<String> = Vec::new();
    let n = phrases.len();
    if n < sc.expect_phrases.0 || n > sc.expect_phrases.1 {
        failures.push(format!("句數 {n} 不在期望 {:?} 內", sc.expect_phrases));
    }
    for kw in sc.expect_keywords {
        if !joined.contains(kw) {
            failures.push(format!("缺關鍵詞 {kw:?}"));
        }
    }
    for f in sc.forbid {
        if joined.contains(f) {
            failures.push(format!("出現禁用字串 {f:?}"));
        }
    }
    // 重複注入偵測：任兩句相同，或串接中同一句出現兩次。
    for (i, (_, a)) in phrases.iter().enumerate() {
        for (_, b) in phrases.iter().skip(i + 1) {
            if a == b && !a.trim().is_empty() {
                failures.push(format!("句子重複注入: {a:?}"));
            }
        }
        if !a.trim().is_empty() && joined.matches(a.as_str()).count() > 1 {
            failures.push(format!("內容重複出現: {a:?}"));
        }
        if is_internally_doubled(a) {
            failures.push(format!("句內重複（×2 退化）: {a:?}"));
        }
    }
    if !cursor_ok {
        failures.push("游標倒退".into());
    }

    println!(
        "  音訊 {total_secs:.1}s | {n} 句 | tick 峰值 {max_tick_ms:.0}ms | 收尾 {final_ms:.0}ms"
    );
    println!("  串接結果:\n---\n{joined}\n---");
    if failures.is_empty() {
        println!("  結果: PASS\n");
        Ok(true)
    } else {
        for f in &failures {
            println!("  FAIL ⚠ {f}");
        }
        println!();
        Ok(false)
    }
}

/// 把場景步驟拼接成單一 16 kHz mono PCM。
fn compose(fixtures: &Path, steps: &[Step]) -> Result<Vec<i16>, RaflowError> {
    let mut pcm = Vec::new();
    for step in steps {
        match step {
            Step::Wav(name) => pcm.extend(read_wav_i16_16k_mono(&fixtures.join(name))?),
            Step::SilenceMs(ms) => pcm.extend(std::iter::repeat_n(0_i16, SAMPLE_RATE * ms / 1_000)),
        }
    }
    Ok(pcm)
}

/// 極簡 WAV 讀取：只接受 16 kHz / mono / 16-bit PCM（與 vad_harness 相同實作）。
fn read_wav_i16_16k_mono(path: &Path) -> Result<Vec<i16>, RaflowError> {
    let bytes = std::fs::read(path).map_err(|e| RaflowError::WhisperLoad {
        detail: format!(
            "讀取 WAV 失敗 {}: {e}（先跑 make tts-fixtures？）",
            path.display()
        ),
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
            return Err(err(format!(
                "chunk `{}` 超出檔案長度",
                String::from_utf8_lossy(id)
            )));
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
