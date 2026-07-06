//! Phase 1 抗幻覺隔離 harness（見 `docs/design/next-mixed-zh-en-streaming.md`）。
//!
//! 目的：在**不碰**現有 pipeline 的前提下，單獨驗證 VAD + suppress_nst + temperature 0 +
//! 無 initial_prompt 的短片段轉錄，是否已把 ADR-0006 的幻覺根因壓掉。
//!
//! 三個關卡：
//!   1. **靜音**（2s 全零）      → 必須輸出**空**（不再冒「字幕製作:貝爾」之類）
//!   2. **雜訊**（2s 決定性白雜訊）→ 空 / 不幻覺
//!   3. **短句中英混講**（可選 WAV）→ 正確轉錄、不回吐 prompt（正確性由人判讀）
//!
//! 用法：
//!   make whisper-vad-model                 # 先備妥 VAD 模型
//!   cargo run -p raflow-speech --example vad_harness --features whisper -- [speech.wav]
//!
//! WAV 需為 16 kHz / mono / 16-bit PCM（用 `afconvert -f WAVE -d LEI16@16000 -c 1` 產出）。

use std::path::Path;

use raflow_core::RaflowError;
use raflow_speech::{WhisperContext, resolve_model_path, resolve_vad_model_path};

/// 每個關卡對輸出的期望。
enum Expect {
    /// 靜音 / 雜訊：任何非空輸出都算幻覺 → FAIL。
    Empty,
    /// 真實語音：非空且無 prompt 回吐才算過；正確性仍由人判讀。
    NonEmpty,
}

fn main() -> Result<(), RaflowError> {
    let model_path = resolve_model_path().ok_or_else(|| RaflowError::WhisperLoad {
        detail: "無法解析 whisper model 路徑（HOME 未設？）".into(),
    })?;
    let vad_path = resolve_vad_model_path().ok_or_else(|| RaflowError::WhisperLoad {
        detail: "無法解析 VAD model 路徑（HOME 未設？）".into(),
    })?;

    println!("== raflow VAD 抗幻覺 harness ==");
    println!("whisper model : {}", model_path.display());
    println!("VAD model     : {}", vad_path.display());
    if !vad_path.exists() {
        return Err(RaflowError::WhisperModelMissing { path: vad_path });
    }
    println!("載入 whisper context …");
    let ctx = WhisperContext::load(&model_path, "zh")?;
    println!("載入完成。\n");

    let sample_rate = 16_000usize;
    let two_seconds = sample_rate * 2;

    let mut all_pass = true;

    // 關卡 1：靜音
    let silence = vec![0i16; two_seconds];
    all_pass &= run_case(&ctx, &vad_path, "靜音 (2s 全零)", &silence, Expect::Empty)?;

    // 關卡 2：決定性白雜訊（LCG，不引 rand crate；固定 seed 讓結果可重現）
    let noise = white_noise(two_seconds, 3000);
    all_pass &= run_case(&ctx, &vad_path, "白雜訊 (2s)", &noise, Expect::Empty)?;

    // 關卡 3：可選真實語音 WAV
    if let Some(wav) = std::env::args().nth(1) {
        let pcm = read_wav_i16_16k_mono(Path::new(&wav))?;
        let label = format!("語音 WAV ({wav})");
        all_pass &= run_case(&ctx, &vad_path, &label, &pcm, Expect::NonEmpty)?;
    } else {
        println!("(未提供 WAV 引數 → 跳過語音準確度關卡；只跑了幻覺關卡)\n");
    }

    println!("== 總結：{} ==", if all_pass { "PASS" } else { "FAIL" });
    Ok(())
}

/// 跑一個關卡，印出原始輸出與 PASS/FAIL，回傳是否通過。
fn run_case(
    ctx: &WhisperContext,
    vad_path: &Path,
    label: &str,
    pcm: &[i16],
    expect: Expect,
) -> Result<bool, RaflowError> {
    let secs = pcm.len() as f32 / 16_000.0;
    let out = ctx.transcribe_streaming(pcm, vad_path)?;
    let trimmed = out.trim();
    let pass = match expect {
        Expect::Empty => trimmed.is_empty(),
        Expect::NonEmpty => !trimmed.is_empty(),
    };
    println!("--- {label} ({secs:.2}s, {} samples) ---", pcm.len());
    println!("輸出: {trimmed:?}");
    println!("結果: {}\n", if pass { "PASS" } else { "FAIL ⚠" });
    Ok(pass)
}

/// 決定性白雜訊：線性同餘產生器（LCG），振幅 ±`amp`。固定 seed → 可重現、不引 rand。
fn white_noise(len: usize, amp: i16) -> Vec<i16> {
    let mut state: u32 = 0x1234_5678;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        // Numerical Recipes LCG 常數
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        // 取高位映射到 [-1, 1) 再乘振幅
        let unit = (state >> 8) as f32 / (1u32 << 24) as f32; // [0,1)
        let sample = ((unit * 2.0 - 1.0) * amp as f32) as i32;
        out.push(sample.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }
    out
}

/// 極簡 WAV 讀取：只接受 16 kHz / mono / 16-bit PCM（afconvert 產出格式）。
/// 逐 chunk 掃 `fmt ` 與 `data`，欄位不符即回明確錯誤（不 panic、不 unwrap）。
fn read_wav_i16_16k_mono(path: &Path) -> Result<Vec<i16>, RaflowError> {
    let bytes = std::fs::read(path).map_err(|e| RaflowError::WhisperLoad {
        detail: format!("讀取 WAV 失敗 {}: {e}", path.display()),
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
            // 1 = PCM；0xFFFE = WAVE_FORMAT_EXTENSIBLE（afconvert 對 16-bit 就是輸出這個，
            // 真正格式在 SubFormat GUID，但對 16-bit LE 整數而言等同 PCM）。
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
        // chunk 以偶數 byte 對齊
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
