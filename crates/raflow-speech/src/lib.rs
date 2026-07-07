mod backend;
mod session;
mod term_restore;
mod whisper_backend;

#[cfg(target_os = "macos")]
mod apple_backend;

pub use backend::SpeechBackend;
pub use session::Session;
pub use term_restore::restore_terms;
pub use whisper_backend::{
    ROLLING_PENDING_SPEECH_FAST_LOCK, ROLLING_TRAILING_SILENCE_FAST,
    ROLLING_TRAILING_SILENCE_SAMPLES, RollingTickOutcome, SAMPLES_PER_CENTISECOND, WhisperContext,
    default_model_path, default_vad_model_path, is_repetition_loop, is_safe_whisper_output,
    normalize_dictation_commands, parse_rolling_flag, resolve_model_path, resolve_vad_model_path,
    rolling_enabled, rolling_tick_core, rolling_trailing_silence_for, segments_pending,
    segments_ready_to_finalize, strip_committed_prefix, vad_cs_to_samples,
};

#[cfg(target_os = "macos")]
pub use apple_backend::{AppleSpeechBackend, request_authorization};
