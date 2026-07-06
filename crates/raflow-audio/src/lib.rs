use cpal::SampleFormat;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use raflow_core::{AudioFrame, RaflowError};
use tokio::sync::mpsc::UnboundedSender;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;
pub const FRAME_MS: u32 = 20;
pub const FRAME_SAMPLES: usize = (TARGET_SAMPLE_RATE as usize) * (FRAME_MS as usize) / 1000;

fn input_samples_per_frame(input_rate: u32) -> usize {
    (input_rate as usize) * (FRAME_MS as usize) / 1000
}

fn capture_error(detail: impl std::fmt::Display) -> RaflowError {
    RaflowError::AudioCapture {
        detail: detail.to_string(),
    }
}

fn mix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    debug_assert!(channels >= 1, "channels must be positive");
    if channels == 1 {
        return interleaved.to_vec();
    }
    let channels = channels as usize;
    let scale = 1.0 / channels as f32;
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() * scale)
        .collect()
}

fn linear_resample_frame(input: &[f32], input_rate: u32) -> Vec<f32> {
    debug_assert_eq!(input.len(), input_samples_per_frame(input_rate));
    debug_assert!(!input.is_empty());
    let step = input_rate as f32 / TARGET_SAMPLE_RATE as f32;
    let last = input.len() - 1;
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = i as f32 * step;
            let idx_floor = (t.floor() as usize).min(last);
            let idx_ceil = (idx_floor + 1).min(last);
            let frac = t - idx_floor as f32;
            input[idx_floor] * (1.0 - frac) + input[idx_ceil] * frac
        })
        .collect()
}

fn f32_sample_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

pub(crate) struct Pipeline {
    input_rate: u32,
    channels: u16,
    buffer: Vec<f32>,
}

impl Pipeline {
    pub fn new(input_rate: u32, channels: u16) -> Self {
        Self {
            input_rate,
            channels,
            buffer: Vec::new(),
        }
    }

    pub fn push_and_drain(&mut self, interleaved: &[f32]) -> Vec<AudioFrame> {
        self.buffer.extend(mix_to_mono(interleaved, self.channels));
        let chunk_len = input_samples_per_frame(self.input_rate);
        let mut frames = Vec::new();
        while self.buffer.len() >= chunk_len {
            let chunk: Vec<f32> = self.buffer.drain(..chunk_len).collect();
            let resampled = linear_resample_frame(&chunk, self.input_rate);
            let pcm = resampled.iter().copied().map(f32_sample_to_i16).collect();
            frames.push(AudioFrame {
                pcm,
                sample_rate: TARGET_SAMPLE_RATE,
            });
        }
        frames
    }
}

pub struct CaptureHandle {
    _stream: cpal::Stream,
}

pub fn start(tx: UnboundedSender<AudioFrame>) -> Result<CaptureHandle, RaflowError> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| capture_error("no default input device"))?;
    let supported = device.default_input_config().map_err(capture_error)?;

    if supported.sample_format() != SampleFormat::F32 {
        return Err(capture_error(format!(
            "unsupported sample format: {:?} (MVP requires f32)",
            supported.sample_format()
        )));
    }

    let input_rate = supported.sample_rate();
    let channels = supported.channels();
    let config: cpal::StreamConfig = supported.into();

    let mut pipeline = Pipeline::new(input_rate, channels);
    let data_callback = move |data: &[f32], _info: &cpal::InputCallbackInfo| {
        for frame in pipeline.push_and_drain(data) {
            if tx.send(frame).is_err() {
                return;
            }
        }
    };
    let error_callback = |err: cpal::StreamError| {
        eprintln!("raflow-audio stream error: {err}");
    };

    let stream = device
        .build_input_stream(&config, data_callback, error_callback, None)
        .map_err(capture_error)?;
    stream.play().map_err(capture_error)?;

    Ok(CaptureHandle { _stream: stream })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_to_mono_passthrough_when_single_channel() {
        let input = vec![0.5_f32, -0.5, 0.25];
        assert_eq!(mix_to_mono(&input, 1), input);
    }

    #[test]
    fn mix_to_mono_averages_multichannel_samples() {
        let cases: Vec<(u16, Vec<f32>, Vec<f32>)> = vec![
            (2, vec![1.0, -1.0, 0.5, 0.5], vec![0.0, 0.5]),
            (3, vec![0.3, 0.6, 0.9, -0.3, -0.6, -0.9], vec![0.6, -0.6]),
        ];
        for (channels, input, expected) in cases {
            let actual = mix_to_mono(&input, channels);
            assert_eq!(actual.len(), expected.len(), "channels={channels}");
            for (a, e) in actual.iter().zip(expected.iter()) {
                assert!((a - e).abs() < 1e-6, "channels={channels}, {a} vs {e}");
            }
        }
    }

    #[test]
    fn input_samples_per_frame_covers_common_rates() {
        let cases = [(16_000_u32, 320_usize), (44_100, 882), (48_000, 960)];
        for (rate, expected) in cases {
            assert_eq!(input_samples_per_frame(rate), expected, "rate={rate}");
        }
    }

    #[test]
    fn linear_resample_preserves_constant_signal() {
        let cases = [(16_000_u32), (44_100), (48_000)];
        for rate in cases {
            let len = input_samples_per_frame(rate);
            let input = vec![0.5_f32; len];
            let output = linear_resample_frame(&input, rate);
            assert_eq!(output.len(), FRAME_SAMPLES, "rate={rate}");
            for (i, s) in output.iter().enumerate() {
                assert!((s - 0.5).abs() < 1e-5, "rate={rate} i={i} sample={s}");
            }
        }
    }

    #[test]
    fn linear_resample_edge_samples_do_not_panic() {
        let rate = 48_000;
        let len = input_samples_per_frame(rate);
        let mut input = vec![0.0_f32; len];
        input[0] = 1.0;
        input[len - 1] = -1.0;
        let output = linear_resample_frame(&input, rate);
        assert_eq!(output.len(), FRAME_SAMPLES);
        assert!((output[0] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn f32_sample_to_i16_handles_range_and_clamps() {
        let cases = [
            (0.0_f32, 0_i16),
            (1.0, i16::MAX),
            (-1.0, -i16::MAX),
            (1.5, i16::MAX),
            (-1.5, -i16::MAX),
        ];
        for (input, expected) in cases {
            assert_eq!(f32_sample_to_i16(input), expected, "input={input}");
        }
    }

    #[test]
    fn pipeline_emits_full_frames_and_buffers_remainder() {
        let mut pipeline = Pipeline::new(48_000, 1);

        let no_frame = pipeline.push_and_drain(&vec![0.0_f32; 500]);
        assert!(no_frame.is_empty(), "insufficient samples must not emit");

        let extra = pipeline.push_and_drain(&vec![0.0_f32; 460]);
        assert_eq!(extra.len(), 1, "500 + 460 = 960 samples → 1 frame");
        assert_eq!(extra[0].pcm.len(), FRAME_SAMPLES);
        assert_eq!(extra[0].sample_rate, TARGET_SAMPLE_RATE);

        let two_frames = pipeline.push_and_drain(&vec![0.0_f32; 1920]);
        assert_eq!(two_frames.len(), 2, "1920 samples → 2 frames");
        for frame in &two_frames {
            assert_eq!(frame.pcm.len(), FRAME_SAMPLES);
            assert_eq!(frame.sample_rate, TARGET_SAMPLE_RATE);
        }
    }

    #[test]
    fn pipeline_handles_multichannel_input() {
        let mut pipeline = Pipeline::new(48_000, 2);
        let stereo = vec![0.0_f32; 960 * 2];
        let frames = pipeline.push_and_drain(&stereo);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].pcm.len(), FRAME_SAMPLES);
    }
}
