use raflow_core::{AudioFrame, RaflowError, TranscriptUpdate};
use tokio::sync::mpsc::UnboundedSender;

pub trait SpeechBackend {
    fn start(
        &mut self,
        locale: &str,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
    ) -> Result<(), RaflowError>;

    fn push_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError>;

    fn stop(&mut self) -> Result<(), RaflowError>;

    /// Phase 2 句級滾動 tick（ADR-0006 §8.7.2）。錄音中由計時器週期呼叫；`is_final=true`
    /// 為錄音停止的收尾 flush（把剩餘語音段全定稿）。
    ///
    /// **no-op 路徑**：非滾動 backend 或 `RAFLOW_ROLLING=0` 時完全不動作，退回「停止時
    /// 整段校正」行為。實作見 `AppleSpeechBackend`（僅在 `rolling` 開啟時作用）。
    fn rolling_tick(&mut self, _is_final: bool) -> Result<(), RaflowError> {
        Ok(())
    }
}
