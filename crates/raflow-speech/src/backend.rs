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

    /// 本 session（`start` 之後）是否為句級滾動——即會產生中途 `PhraseFinal` 段界。
    /// 供 Edit Guard 判定是否啟用（只在有段界可作恢復錨點的 session 啟用）。
    /// 預設 `false`（非滾動 backend / fake）；`AppleSpeechBackend` 依 `session_rolling` 回報。
    fn session_rolling(&self) -> bool {
        false
    }
}
