#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub pcm: Vec<i16>,
    pub sample_rate: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_holds_pcm_and_rate() {
        let frame = AudioFrame {
            pcm: vec![0, 1, -1],
            sample_rate: 16_000,
        };
        assert_eq!(frame.pcm.len(), 3);
        assert_eq!(frame.sample_rate, 16_000);
    }

    #[test]
    fn frame_equality_requires_matching_rate_and_pcm() {
        let a = AudioFrame {
            pcm: vec![0, 1],
            sample_rate: 16_000,
        };
        let b = AudioFrame {
            pcm: vec![0, 1],
            sample_rate: 16_000,
        };
        let c = AudioFrame {
            pcm: vec![0, 1],
            sample_rate: 48_000,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
