use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RaflowError {
    #[error("failed to load config from {path}")]
    ConfigLoad {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to register hotkey: {detail}")]
    HotkeyRegister { detail: String },
    #[error("audio capture failed: {detail}")]
    AudioCapture { detail: String },
    #[error("speech authorization failed: {status}")]
    SpeechAuthorization { status: String },
    #[error("speech recognition not available for locale {locale}")]
    SpeechUnavailable { locale: String },
    #[error("speech session is already running")]
    SpeechBusy,
    #[error("text injection failed: {detail}")]
    TextInject { detail: String },
    #[error("clipboard write failed: {detail}")]
    ClipboardWrite { detail: String },
    #[error("whisper model not found at {path}")]
    WhisperModelMissing { path: PathBuf },
    #[error("whisper context load failed: {detail}")]
    WhisperLoad { detail: String },
    #[error("whisper inference failed: {detail}")]
    WhisperInference { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::io;

    fn sample_config_load_err() -> RaflowError {
        RaflowError::ConfigLoad {
            path: PathBuf::from("/tmp/missing.toml"),
            source: io::Error::new(io::ErrorKind::NotFound, "not found"),
        }
    }

    #[test]
    fn config_load_display_contains_path() {
        let err = sample_config_load_err();
        assert!(
            err.to_string().contains("/tmp/missing.toml"),
            "display should embed the failing path, got: {err}"
        );
    }

    #[test]
    fn config_load_preserves_source_chain() {
        let err = sample_config_load_err();
        assert!(
            err.source().is_some(),
            "#[source] must expose the underlying io::Error"
        );
    }

    #[test]
    fn hotkey_register_display_contains_detail() {
        let err = RaflowError::HotkeyRegister {
            detail: "already registered".to_string(),
        };
        assert!(
            err.to_string().contains("already registered"),
            "display should embed the failure detail, got: {err}"
        );
    }

    #[test]
    fn audio_capture_display_contains_detail() {
        let err = RaflowError::AudioCapture {
            detail: "no default input device".to_string(),
        };
        assert!(
            err.to_string().contains("no default input device"),
            "display should embed the failure detail, got: {err}"
        );
    }

    #[test]
    fn speech_error_displays_have_expected_shape() {
        let cases: Vec<(RaflowError, &str)> = vec![
            (
                RaflowError::SpeechAuthorization {
                    status: "denied".into(),
                },
                "denied",
            ),
            (
                RaflowError::SpeechUnavailable {
                    locale: "zh-TW".into(),
                },
                "zh-TW",
            ),
            (RaflowError::SpeechBusy, "already running"),
        ];
        for (err, needle) in cases {
            assert!(
                err.to_string().contains(needle),
                "display should embed {needle:?}, got: {err}"
            );
        }
    }

    #[test]
    fn text_inject_display_contains_detail() {
        let err = RaflowError::TextInject {
            detail: "accessibility permission denied".into(),
        };
        assert!(
            err.to_string().contains("accessibility permission denied"),
            "display should embed injection failure detail, got: {err}"
        );
    }

    #[test]
    fn clipboard_write_display_contains_detail() {
        let err = RaflowError::ClipboardWrite {
            detail: "NSPasteboard unavailable".into(),
        };
        assert!(
            err.to_string().contains("NSPasteboard unavailable"),
            "display should embed clipboard failure detail, got: {err}"
        );
    }

    #[test]
    fn whisper_errors_have_expected_shape() {
        let cases: Vec<(RaflowError, &str)> = vec![
            (
                RaflowError::WhisperModelMissing {
                    path: PathBuf::from("/Users/me/models/ggml-small.bin"),
                },
                "/Users/me/models/ggml-small.bin",
            ),
            (
                RaflowError::WhisperLoad {
                    detail: "ggml format invalid".into(),
                },
                "ggml format invalid",
            ),
            (
                RaflowError::WhisperInference {
                    detail: "decoder OOM".into(),
                },
                "decoder OOM",
            ),
        ];
        for (err, needle) in cases {
            assert!(
                err.to_string().contains(needle),
                "display should embed {needle:?}, got: {err}"
            );
        }
    }
}
