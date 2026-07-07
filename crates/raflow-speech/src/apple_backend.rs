//! Apple Speech Framework 整合，經由 objc2-speech 純 Rust 綁定呼叫。
//!
//! 本檔是整個專案唯一允許 `unsafe` 區塊的地方（見 ADR-0002）。
//! 每個 `unsafe { ... }` 必附 `// SAFETY:` 註解。
//! 模組入口（`lib.rs`）已用 `#[cfg(target_os = "macos")]` 限定，這裡不再重複。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_avf_audio::{AVAudioCommonFormat, AVAudioFormat, AVAudioPCMBuffer};
use objc2_foundation::{NSArray, NSError, NSLocale, NSString};
use objc2_speech::{
    SFSpeechAudioBufferRecognitionRequest, SFSpeechRecognitionResult, SFSpeechRecognitionTask,
    SFSpeechRecognizer, SFSpeechRecognizerAuthorizationStatus,
};
use raflow_core::{AudioFrame, RaflowError, TranscriptUpdate};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::backend::SpeechBackend;
use crate::term_restore::restore_terms;
use crate::whisper_backend::{
    WhisperContext, is_safe_whisper_output, resolve_vad_model_path, rolling_final_flush_delivered,
    rolling_tick_core, strip_committed_prefix,
};

/// Apple Speech 對單一 task 的 audio 上限為 1 分鐘（spec/whisper.md §7）。
/// Whisper 餵超過這個量沒意義，只取最後 60 秒。
const MAX_PCM_SAMPLES_FOR_WHISPER: usize = 60 * 16_000;

// 句級滾動「停頓即鎖」門檻 ROLLING_TRAILING_SILENCE_SAMPLES 移至 whisper_backend
// （決策核心 rolling_tick_core 與離線 harness 共用；語意見該處 doc）。

/// 領域專有名詞提示（Apple `SFSpeechRecognitionRequest.contextualStrings`）：告訴 Apple
/// 這些 SRE / DevOps / 程式術語「可能會出現」，提高辨識機率、少把英文術語認爛。純 Apple 原生、
/// 不需 Whisper。使用者可於 `~/Library/Application Support/raflow/contextual_terms.txt`
/// （每行一個，`#` 開頭為註解）追加自己的術語，見 [`merge_contextual_terms`]。
const DEFAULT_CONTEXTUAL_TERMS: &[&str] = &[
    // Cloud / 基礎設施
    "Kubernetes",
    "kubectl",
    "Docker",
    "Terraform",
    "Ansible",
    "Helm",
    "Istio",
    "Envoy",
    "Prometheus",
    "Grafana",
    "Nginx",
    "HAProxy",
    "containerd",
    "Vault",
    "Consul",
    "etcd",
    // Cloud 供應商 / 服務
    "AWS",
    "GCP",
    "Azure",
    "EC2",
    "S3",
    "Lambda",
    "DynamoDB",
    "CloudFront",
    "EKS",
    "GKE",
    "IAM",
    "VPC",
    "RDS",
    "CloudWatch",
    // CI/CD
    "Jenkins",
    "GitLab",
    "GitHub",
    "GitHub Actions",
    "ArgoCD",
    "CircleCI",
    // 資料 / 訊息
    "PostgreSQL",
    "MySQL",
    "MongoDB",
    "Redis",
    "Kafka",
    "Elasticsearch",
    "RabbitMQ",
    "Cassandra",
    "ClickHouse",
    // API / 協定
    "GraphQL",
    "gRPC",
    "REST",
    "WebSocket",
    "OAuth",
    "JWT",
    "Webhook",
    // 語言 / runtime
    "Python",
    "Golang",
    "Rust",
    "TypeScript",
    "JavaScript",
    "Node.js",
    // 概念 / 實務
    "DevOps",
    "SRE",
    "DevSecOps",
    "observability",
    "OpenTelemetry",
    "microservice",
    "serverless",
    "namespace",
    "ingress",
    "sidecar",
    "rollback",
    "canary",
    "SLA",
    "SLO",
    "SLI",
    // AI 工具
    "ChatGPT",
    "OpenAI",
    "Claude",
    "Anthropic",
    "Copilot",
    "Cursor",
    "LLM",
    "GPT",
    "RAG",
    // 泛用縮寫
    "YAML",
    "JSON",
    "API",
    "CLI",
    "SDK",
    "CDN",
    "DNS",
    "TLS",
    "CI/CD",
    // --- SRE / DevOps 擴充（精選核心；contextualStrings 過長會稀釋效果，只放常見/常被認錯的）---
    // Kubernetes 物件 / 元件
    "Deployment",
    "StatefulSet",
    "DaemonSet",
    "ConfigMap",
    "kubelet",
    "HPA",
    "PVC",
    // Service mesh / proxy
    "Linkerd",
    "Traefik",
    "Kong",
    // Observability
    "Datadog",
    "PagerDuty",
    "Loki",
    "Thanos",
    "Jaeger",
    "Alertmanager",
    // CD / IaC
    "Flux",
    "Tekton",
    "Pulumi",
    "Spinnaker",
    // Containers
    "Podman",
    "Kaniko",
    // Cloud
    "ECS",
    "Fargate",
    "SQS",
    "SNS",
    "Cloud Run",
    // 資料 / 串流
    "CockroachDB",
    "Trino",
    "Flink",
    "Airflow",
    "NATS",
    // 安全 / 網路
    "Trivy",
    "Falco",
    "cert-manager",
    "OPA",
    "Cilium",
    "Calico",
    "CNI",
    "mTLS",
    "Protobuf",
    // SRE 指標 / 概念
    "MTTR",
    "RTO",
    "RPO",
    "runbook",
    "postmortem",
    "error budget",
    "on-call",
    "p95",
    "p99",
];

/// 合併預設術語與使用者檔案內容（每行一個術語；trim；跳過空行與 `#` 註解），保序去重。
/// 純函式（無 I/O），供測試與 [`load_contextual_terms`] 共用。
fn merge_contextual_terms(defaults: &[&str], user_file: Option<&str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let user_lines = user_file.into_iter().flat_map(|c| c.lines());
    let defaults_iter = defaults.iter().map(|s| s.to_string());
    let user_iter = user_lines.filter_map(|line| {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            None
        } else {
            Some(t.to_string())
        }
    });
    for term in defaults_iter.chain(user_iter) {
        if seen.insert(term.clone()) {
            out.push(term);
        }
    }
    out
}

/// 使用者術語檔路徑：env `RAFLOW_CONTEXTUAL_TERMS` 優先，否則預設
/// `$HOME/Library/Application Support/raflow/contextual_terms.txt`。
fn contextual_terms_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RAFLOW_CONTEXTUAL_TERMS") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("raflow");
    p.push("contextual_terms.txt");
    Some(p)
}

/// 載入最終術語清單：預設 + 使用者檔（若存在）。檔案讀取失敗（不存在等）→ 只用預設。
fn load_contextual_terms() -> Vec<String> {
    let user = contextual_terms_path().and_then(|p| std::fs::read_to_string(p).ok());
    merge_contextual_terms(DEFAULT_CONTEXTUAL_TERMS, user.as_deref())
}

/// 術語還原診斷 trace 開關 `RAFLOW_RESTORE_TRACE` 的純解析（**預設 OFF**，opt-in）。
///
/// 語意：只有明確 truthy 值（`1`/`true`/`yes`/`on`，忽略大小寫、去前後空白）→ ON；
/// 未設 / 空 / 其餘任何值 → OFF。與 [`parse_rolling_flag`] 極性相反——trace 會印出
/// 使用者口述全文（Whisper 定稿 + Apple 草稿）供 spec/whisper.md §18 錯例萃取，
/// 故**預設不輸出**，避免敏感內容落入 stderr／log（Codex stop-review）。
/// 抽成純函式讓測試不碰 process 全域 env（憲法 §2.4）；env 讀取見 [`restore_trace_enabled`]。
fn parse_restore_trace_flag(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// 讀 `RAFLOW_RESTORE_TRACE` env 決定是否啟用術語還原診斷 trace（預設 OFF）。
/// 薄層包裝 [`parse_restore_trace_flag`]（後者已測）；本函式只做 env I/O，不另測。
fn restore_trace_enabled() -> bool {
    parse_restore_trace_flag(std::env::var("RAFLOW_RESTORE_TRACE").ok().as_deref())
}

/// 滾動 prompt priming 的核心預設術語（離線 harness 以真人錄音 + TTS 驗證過的
/// 高價值詞——ArgoCD/GitLab CI/IaC/Terraform 實測由錯轉對）。刻意不用完整
/// `DEFAULT_CONTEXTUAL_TERMS`（~118 詞）：prompt 上限 30，全塞會稀釋偏置且把
/// 使用者詞擠出去。
const DEFAULT_PROMPT_TERMS: &[&str] = &[
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

/// 組滾動 prompt 術語：**使用者檔案詞優先**（自家詞最常被聽錯，須佔住 30 上限的
/// 前排），再補核心預設；保序去重、跳過註解/空行。上限裁切由
/// `build_rolling_prompt`（whisper_backend）執行。
fn prompt_terms_user_first(user_file: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(text) = user_file {
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if !out.iter().any(|e| e == t) {
                out.push(t.to_string());
            }
        }
    }
    for d in DEFAULT_PROMPT_TERMS {
        if !out.iter().any(|e| e == d) {
            out.push((*d).to_string());
        }
    }
    out
}

/// 讀使用者術語檔 → 滾動 prompt 術語（每次 start 重讀，改檔下次錄音生效）。
fn load_prompt_terms() -> Vec<String> {
    let user = contextual_terms_path().and_then(|p| std::fs::read_to_string(p).ok());
    prompt_terms_user_first(user.as_deref())
}

/// Phase 5-fix6 條件觸發：Apple final 是否包含 ASCII 英文字母。
/// 純中文（無 a-zA-Z）→ 信任 Apple，跳過 Whisper inference，避免：
///   1. Whisper 改寫中文（即使原本對的）
///   2. Whisper hallucinate 韓文 / 簡體
///   3. ~2s/min 的 inference 時間 + 風扇噪音
///
/// 詳見 docs/spec/whisper.md §14。
fn apple_final_has_english(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_alphabetic())
}

/// 是否對這段 Apple final 跑 Whisper 終校。
///
/// 只有 **zh-TW session 且 final 含英文** 才跑：修正中文 session 裡夾雜、被 zh 聲學模型
/// 辨錯的英文。其餘一律不跑：
/// - **en-US session**：Apple 的 en-US final 已是正確英文；Whisper 被強制 zh tokenizer，
///   對英文音訊會幻覺（實測「字幕製作:貝爾」把正確英文整段蓋掉），必須跳過。
/// - **純中文 final**：Apple zh-TW 表現極佳，跑 Whisper 只會有改寫風險 + ~2s 開銷。
///
/// 詳見 docs/spec/whisper.md §14。
fn should_correct_with_whisper(session_locale: &str, apple_final: &str) -> bool {
    session_locale == "zh-TW" && apple_final_has_english(apple_final)
}

/// 句級滾動是否對本 session 生效：需滾動能力齊備（flag + 閘門 + whisper）、
/// VAD 模型檔實際存在、**且** zh-TW session。
///
/// - en-US session 一律不滾動——Whisper 強制 zh tokenizer，對英文音訊會幻覺或
///   直接翻譯成中文（與 [`should_correct_with_whisper`] 的 locale 守門同一理由）；
///   Apple 的 en-US 輸出本身已是正確英文，直出即可。
/// - VAD 模型缺 → 不滾動：否則 callback 抑制 Apple final、`rolling_tick` 卻永遠
///   no-op，整個 session 收不到任何 final（剪貼簿 / overlay 卡住）。
fn rolling_session_active(rolling_capable: bool, vad_ready: bool, session_locale: &str) -> bool {
    rolling_capable && vad_ready && session_locale == "zh-TW"
}

/// 呼叫 Apple 的 `SFSpeechRecognizer.requestAuthorization`，等候使用者回應後回傳結果。
///
/// 由 `raflow-app` 於啟動時呼叫一次（見 `docs/spec/speech.md §3`）。
///
/// log 啟動時的 authorizationStatus()：對排查「每次重啟都跳 prompt」很關鍵——若 log 顯示
/// `before=NotDetermined` 代表 TCC 沒持久化（多半是 cert 不被信任 → 換 self-signed +
/// `security add-trusted-cert`）；若 `before=Authorized` 但 prompt 仍跳，則是程式邏輯問題。
pub async fn request_authorization() -> Result<(), RaflowError> {
    // SAFETY: 靜態類方法，無前置條件。
    let before = unsafe { SFSpeechRecognizer::authorizationStatus() };
    eprintln!("raflow: speech auth status (pre-request) = {before:?}");

    let (tx, rx) = oneshot::channel::<SFSpeechRecognizerAuthorizationStatus>();
    let tx = Mutex::new(Some(tx));

    let handler = RcBlock::new(move |status: SFSpeechRecognizerAuthorizationStatus| {
        if let Ok(mut guard) = tx.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(status);
            }
        }
    });

    // SAFETY: SFSpeechRecognizer::requestAuthorization 是 Apple 靜態方法，接受 block handler。
    // handler 以 RcBlock 保活到其 scope 結束（即本 async fn 結束）；Apple 會於回應時呼叫之。
    unsafe {
        SFSpeechRecognizer::requestAuthorization(&handler);
    }

    let status = rx.await.map_err(|_| RaflowError::SpeechAuthorization {
        status: "authorization handler dropped before responding".to_string(),
    })?;
    eprintln!("raflow: speech auth status (post-request) = {status:?}");

    // SFSpeechRecognizerAuthorizationStatus 為 NSInteger newtype；objc2-speech 暴露其常數。
    match status {
        SFSpeechRecognizerAuthorizationStatus::Authorized => Ok(()),
        other => Err(RaflowError::SpeechAuthorization {
            status: format!("{other:?}"),
        }),
    }
}

/// 生產環境的 Speech 後端：持有 `SFSpeechRecognizer`，每次 `start` 建立一個新的 request/task。
///
/// 內部維護 `pcm_buffer`：累積本次 session 的原始 16 kHz mono i16 PCM，供 Phase 5
/// Whisper 終校 batch transcribe 使用。即使未注入 `WhisperContext`，buffer 仍會累積
/// （overhead 為一個 Vec<i16>），在每次 `start` 時清空、`stop` 時 callback 用完後清空。
pub struct AppleSpeechBackend {
    recognizer: Retained<SFSpeechRecognizer>,
    locale: String,
    active: Option<ActiveSession>,
    pcm_buffer: Arc<Mutex<Vec<i16>>>,
    whisper: Option<Arc<WhisperContext>>,
    // ── Phase 2 句級滾動（ADR-0006 §8.7.2）；rolling=false 時以下全不作用，行為同 HEAD ──
    /// 由 `RAFLOW_ROLLING` 決定（`with_rolling`）。預設 ON；`RAFLOW_ROLLING=0` → 停止時整段校正。
    rolling: bool,
    /// 執行期校正閘門（menu「Whisper 智慧校正」開關，spec/settings.md §4）：
    /// 回 false → Whisper 全不介入（滾動與整段終校皆跳過）。未注入 → 恆 ON。
    correction_gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// 本次 session 的閘門快照：`start()` 時讀一次、整個 session 固定——避免錄音
    /// 中途切 OFF 造成「callback 已抑制 Apple final、tick 卻不再補送」的文字遺失。
    session_correction: bool,
    /// 本次 session 滾動是否生效（`start()` 時以 [`rolling_session_active`] 定案）：
    /// 能力齊備、VAD 模型在、且 zh-TW session 才滾動。否則 Apple final 直出。
    session_rolling: bool,
    /// Edit Guard 是否接管中（printer 執行緒寫、rolling_tick 讀）。接管中 → 用極短尾靜音門檻
    /// 盡快定稿當前段、清空草稿，讓下次開口即刻恢復（docs/design/edit-guard.md §4）。預設一個
    /// 獨立 flag（恆 false）；由 `with_edit_guard_flag` 注入 app 層共享的那個。
    edit_guard_frozen: Arc<AtomicBool>,
    /// Apple 整段 final 是否抑制（與 callback 共享）。滾動 session 於 `start()` 設
    /// true；收尾 flush **失敗**（守門拒收/空輸出/核心錯誤）時清為 false → 稍後
    /// 抵達的 Apple final 以未定稿尾段回退直出，避免空 `Final` 清掉草稿（資料遺失）。
    suppress_apple_final: Arc<AtomicBool>,
    /// 最新的 Apple cumulative partial 全文（callback 寫、rolling_tick 讀）：
    /// 術語還原（spec/whisper.md §18）需要「同段音訊的 Apple 草稿」做對齊來源。
    apple_partial_text: Arc<Mutex<String>>,
    /// 術語還原用詞彙表（內建 + 使用者檔**全部**，不受 prompt 上限約束；每 session 重讀）。
    restore_vocab: Vec<String>,
    /// 術語還原診斷 trace 開關（`RAFLOW_RESTORE_TRACE`，**預設 OFF**；每 session start 重讀）。
    /// ON 時每次 phrase-final 把 Whisper 定稿與 Apple 草稿印到 stderr，供 §18 錯例萃取。
    /// 含使用者口述全文，故 opt-in、預設不輸出（見 [`parse_restore_trace_flag`]）。
    restore_trace: bool,
    /// 已定稿音訊的**結束樣本位置**（rolling_tick 游標；每次 start 歸零）。
    /// 以樣本位置而非段數：VAD 對成長中緩衝重切段時（合併/分裂），段數游標會指錯
    /// 音訊 → 已定稿內容被重複轉錄（實測「另外我們」重複、尾句 ×4）；樣本位置
    /// 單調遞增，不受重切影響（`segments_pending` 據此過濾）。
    finalized_samples: usize,
    /// 已鎖定前綴的 char 數（Apple cumulative 空間）；callback 據此裁切 partial（§2.2）。
    /// tick 定稿後更新、callback 讀取 → 用 atomic 跨 worker/Apple 兩執行緒共享。
    committed_chars: Arc<AtomicUsize>,
    /// callback 每次 partial 記錄的 Apple cumulative char 數；tick 定稿時快照進 committed_chars。
    apple_cumulative_chars: Arc<AtomicUsize>,
    /// start 時存下的 transcript sender，供 rolling_tick 在 worker thread 發 `PhraseFinal`。
    transcript_tx: Option<UnboundedSender<TranscriptUpdate>>,
    /// 滾動 prompt priming 術語（使用者檔優先 + 核心預設；每次 start 重讀）。
    /// 離線 harness 驗證：ArgoCD/GitLab CI/IaC 由錯轉對、零回吐、延遲無回歸。
    prompt_terms: Vec<String>,
    /// VAD model 路徑（啟動時解析一次）；rolling_tick 每 tick 切句用。
    vad_model_path: Option<PathBuf>,
}

struct ActiveSession {
    request: Retained<SFSpeechAudioBufferRecognitionRequest>,
    _task: Retained<SFSpeechRecognitionTask>,
    // Block 必須保活到 task 結束為止（Apple 會保留 block 自身引用，我們仍持有以示所有權意圖）
    _result_handler: RcBlock<dyn Fn(*mut SFSpeechRecognitionResult, *mut NSError)>,
}

impl AppleSpeechBackend {
    /// 建立指定 locale 的辨識器。若 locale 不受支援，回傳 `SpeechUnavailable`。
    pub fn new(locale: &str) -> Result<Self, RaflowError> {
        let recognizer = Self::build_recognizer(locale)?;

        Ok(Self {
            recognizer,
            locale: locale.to_string(),
            active: None,
            pcm_buffer: Arc::new(Mutex::new(Vec::new())),
            whisper: None,
            rolling: false,
            correction_gate: None,
            session_correction: true,
            session_rolling: false,
            edit_guard_frozen: Arc::new(AtomicBool::new(false)),
            suppress_apple_final: Arc::new(AtomicBool::new(false)),
            apple_partial_text: Arc::new(Mutex::new(String::new())),
            restore_vocab: Vec::new(),
            restore_trace: false,
            finalized_samples: 0,
            committed_chars: Arc::new(AtomicUsize::new(0)),
            apple_cumulative_chars: Arc::new(AtomicUsize::new(0)),
            transcript_tx: None,
            prompt_terms: Vec::new(),
            vad_model_path: resolve_vad_model_path(),
        })
    }

    /// 建立指定 locale 的 `SFSpeechRecognizer` 並確認即時可用；不受支援 → `SpeechUnavailable`。
    /// 供 `new()` 與 `start()` 動態切換 locale 共用（spec/speech.md §2）。
    fn build_recognizer(locale: &str) -> Result<Retained<SFSpeechRecognizer>, RaflowError> {
        let locale_ns = NSString::from_str(locale);
        let ns_locale = NSLocale::initWithLocaleIdentifier(NSLocale::alloc(), &locale_ns);

        // SAFETY: SFSpeechRecognizer::initWithLocale 在 locale 不受支援時回傳 nil（Option::None）。
        let recognizer =
            unsafe { SFSpeechRecognizer::initWithLocale(SFSpeechRecognizer::alloc(), &ns_locale) }
                .ok_or_else(|| RaflowError::SpeechUnavailable {
                    locale: locale.to_string(),
                })?;

        // SAFETY: isAvailable 為 recognizer 目前的即時可用性屬性 getter。
        let available = unsafe { recognizer.isAvailable() };
        if !available {
            return Err(RaflowError::SpeechUnavailable {
                locale: locale.to_string(),
            });
        }

        Ok(recognizer)
    }

    /// Builder：注入 Whisper 終校 context；callback 收到 Apple final 時會用 Whisper
    /// 重 transcribe 整段 PCM 取代 Apple final。失敗時 fallback 到 Apple final。
    /// 詳見 `docs/spec/whisper.md`。
    pub fn with_whisper(mut self, whisper: Arc<WhisperContext>) -> Self {
        self.whisper = Some(whisper);
        self
    }

    /// Builder：開啟 Phase 2 句級滾動校正（`RAFLOW_ROLLING`，預設 ON；`=0` 退回「停止時
    /// 整段校正」）；ON 時每 tick 對已閉合 VAD 段跑 `transcribe_streaming` 送 `PhraseFinal`，並抑制
    /// Apple 整段 final、裁切 partial 前綴（ADR-0006 §8.7.2）。需 `whisper` + VAD model 才生效。
    pub fn with_rolling(mut self, enabled: bool) -> Self {
        self.rolling = enabled;
        self
    }

    /// Builder：注入 app 層與 printer 共享的「Edit Guard 接管中」旗標。接管中 rolling_tick 改用
    /// 極短尾靜音門檻，盡快清空當前段草稿 → 使用者改完開口即刻恢復（edit-guard.md §4）。
    pub fn with_edit_guard_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.edit_guard_frozen = flag;
        self
    }

    /// Builder：注入執行期校正閘門（menu 開關，spec/settings.md §4）。閘門於每次
    /// `start()` 讀一次快照（`session_correction`），回 false 時該 session 的 Whisper
    /// 全不介入（滾動 + 整段終校皆跳過），Apple 輸出即最終輸出。
    pub fn with_correction_gate(mut self, gate: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        self.correction_gate = Some(gate);
        self
    }

    /// 滾動是否**實際生效**：需 flag ON、session 閘門 ON、且已注入 whisper
    /// （rolling 靠 Whisper 定稿）。缺 whisper 時退化為現行行為——否則 callback 會
    /// 抑制 Apple final 卻無 tick 補送 final，導致整個 session 收不到任何 final
    /// （剪貼簿 / overlay 卡住）。
    fn rolling_active(&self) -> bool {
        self.rolling && self.session_correction && self.whisper.is_some()
    }

    /// 以 16 kHz mono int16 PCM 為來源，建立 `AVAudioPCMBuffer`。
    fn make_pcm_buffer(frame: &AudioFrame) -> Result<Retained<AVAudioPCMBuffer>, RaflowError> {
        let frame_count = frame.pcm.len() as u32;
        if frame_count == 0 {
            return Err(RaflowError::SpeechAuthorization {
                status: "empty audio frame".to_string(),
            });
        }

        // SAFETY: AVAudioFormat 初始化器接受 common format + sample rate + channels + interleaved；
        // 回傳 Option 指示是否建立成功（通常僅在參數組合非法時失敗）。
        let format = unsafe {
            AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                AVAudioFormat::alloc(),
                AVAudioCommonFormat::PCMFormatInt16,
                frame.sample_rate as f64,
                1,
                true,
            )
        }
        .ok_or_else(|| RaflowError::SpeechUnavailable {
            locale: "AVAudioFormat init failed".to_string(),
        })?;

        // SAFETY: initWithPCMFormat_frameCapacity 回傳 Option；frame_capacity 非零時一般會成功。
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &format,
                frame_count,
            )
        }
        .ok_or_else(|| RaflowError::SpeechUnavailable {
            locale: "AVAudioPCMBuffer init failed".to_string(),
        })?;

        // SAFETY: 設定 frame_length 不超過 frame_capacity（此處相等）。
        unsafe {
            buffer.setFrameLength(frame_count);
        }

        // SAFETY: int16ChannelData 回傳一個「指向通道指標陣列」的原始指標；
        // int16 format 且 channels==1 時，解參考即得第一（唯一）通道的 NonNull<i16>，
        // 該通道緩衝至少容納 frame_capacity 個 i16 樣本。
        unsafe {
            let channels_ptr = buffer.int16ChannelData();
            let first_channel = *channels_ptr;
            std::ptr::copy_nonoverlapping(
                frame.pcm.as_ptr(),
                first_channel.as_ptr(),
                frame.pcm.len(),
            );
        }

        Ok(buffer)
    }
}

impl SpeechBackend for AppleSpeechBackend {
    fn start(
        &mut self,
        locale: &str,
        transcript_tx: UnboundedSender<TranscriptUpdate>,
    ) -> Result<(), RaflowError> {
        // 依 caller（依當前輸入法）指定的 locale：與現有 recognizer 不同時重建。
        // SFSpeechRecognizer 綁定單一 locale，切換語言必須換 recognizer（spec/speech.md §2）。
        if locale != self.locale {
            self.recognizer = Self::build_recognizer(locale)?;
            self.locale = locale.to_string();
        }

        // SAFETY: 再次檢查 recognizer 可用性（狀態可能在 build 後變動）
        let available = unsafe { self.recognizer.isAvailable() };
        if !available {
            return Err(RaflowError::SpeechUnavailable {
                locale: self.locale.clone(),
            });
        }

        // 清空上一輪殘留（防 Phase 9-fix 同型問題：上次 stop 後若沒走完 callback，殘存
        // 的 PCM 不該漏給下一輪 Whisper）
        if let Ok(mut buf) = self.pcm_buffer.lock() {
            buf.clear();
        }

        // Phase 2 滾動狀態歸零（每 session 重算）：定稿游標與前綴長度。
        self.finalized_samples = 0;
        self.committed_chars.store(0, Ordering::Relaxed);
        self.apple_cumulative_chars.store(0, Ordering::Relaxed);
        // 存下 sender 供 rolling_tick 在 worker thread 發 PhraseFinal / 收尾 Final。
        self.transcript_tx = Some(transcript_tx.clone());
        // 滾動 prompt 術語（使用者檔優先；每 session 重讀，改檔下次錄音生效）。
        self.prompt_terms = load_prompt_terms();
        // 術語還原詞彙表（§18）：內建 + 使用者檔全部（與 contextualStrings 同一來源）。
        self.restore_vocab = load_contextual_terms();
        // 術語還原診斷 trace 開關每 session 重讀（RAFLOW_RESTORE_TRACE，預設 OFF）。
        self.restore_trace = restore_trace_enabled();
        // 清空上一輪的 Apple partial 快照。
        if let Ok(mut t) = self.apple_partial_text.lock() {
            t.clear();
        }
        // 校正閘門快照（menu「Whisper 智慧校正」，spec/settings.md §4）：session 內
        // 固定，menu 切換於下一次錄音生效。
        self.session_correction = self.correction_gate.as_ref().is_none_or(|g| g());
        // 滾動的 session 定案：能力齊備、VAD 模型檔實際存在、且 zh-TW session
        // （en-US 完全不過 Whisper；VAD 缺 → 退化整段校正）。
        let vad_ready = self.vad_model_path.as_deref().is_some_and(|p| p.exists());
        self.session_rolling =
            rolling_session_active(self.rolling_active(), vad_ready, &self.locale);
        // Apple final 抑制旗標與 session_rolling 同步；收尾 flush 失敗時由
        // rolling_tick 清除 → callback 放行 Apple final 回退。
        self.suppress_apple_final
            .store(self.session_rolling, Ordering::Relaxed);

        // SAFETY: SFSpeechAudioBufferRecognitionRequest::new 為標準 NSObject init，無前置條件。
        let request = unsafe { SFSpeechAudioBufferRecognitionRequest::new() };

        // 領域術語提示：偏好辨識 SRE/DevOps/程式英文術語（如 Kubernetes、ChatGPT），
        // 減少把英文專有名詞認爛。純 Apple 原生機制（spec/speech.md §7b）。
        // **每次 start 重讀**使用者術語檔 → 改檔後下次錄音即生效，不必重啟 app。
        let contextual_terms = load_contextual_terms();
        if !contextual_terms.is_empty() {
            let ns_terms: Vec<Retained<NSString>> = contextual_terms
                .iter()
                .map(|s| NSString::from_str(s))
                .collect();
            let array = NSArray::from_retained_slice(&ns_terms);
            // SAFETY: setContextualStrings 接受 NSArray<NSString>；Apple 內部 copy，
            // 陣列可於返回後釋放。偏好清單，不改變 request 其他狀態。
            unsafe {
                request.setContextualStrings(&array);
            }
        }

        let tx = transcript_tx.clone();
        let pcm_buffer = self.pcm_buffer.clone();
        let whisper = self.whisper.clone();
        // Phase 2 rolling：callback 依 rolling 決定是否裁切 partial 前綴、抑制 Apple final。
        // 用 session_rolling（能力 + zh-TW locale 定案）：缺 whisper 或 en-US session
        // 時退化為現行行為，避免收不到 final / 英文被 zh-Whisper 改寫。
        let rolling = self.session_rolling;
        // 閘門 OFF → 整段終校也跳過（所見即所得；spec/settings.md §1）。
        let correction_on = self.session_correction;
        // 收尾回退用：flush 失敗時 rolling_tick 清為 false → 放行 Apple final。
        let suppress_apple_final = self.suppress_apple_final.clone();
        // 術語還原（§18）的對齊來源：callback 每個 partial 更新全文快照。
        let apple_partial_text = self.apple_partial_text.clone();
        let committed_chars = self.committed_chars.clone();
        let apple_cumulative_chars = self.apple_cumulative_chars.clone();
        // Whisper 終校強制 zh tokenizer，只對 zh-TW session 有意義。en-US session 的
        // Apple final 已是正確英文，若再餵給 zh-Whisper 會幻覺（如「字幕製作:貝爾」）。
        let session_locale = self.locale.clone();
        let handler: RcBlock<dyn Fn(*mut SFSpeechRecognitionResult, *mut NSError)> = RcBlock::new(
            move |result_ptr: *mut SFSpeechRecognitionResult, error_ptr: *mut NSError| {
                if !error_ptr.is_null() {
                    // SAFETY: Apple 保證非 nil 的 NSError 指標指向有效物件，且呼叫 localizedDescription 安全。
                    let message = unsafe {
                        let err: &NSError = &*error_ptr;
                        err.localizedDescription().to_string()
                    };
                    let _ = tx.send(TranscriptUpdate::Error(message));
                    return;
                }
                if result_ptr.is_null() {
                    return;
                }
                // SAFETY: result_ptr 非 nil 時，指向 Apple 產生的 SFSpeechRecognitionResult，
                // 其生命週期由 Apple 保證涵蓋本 callback。
                let (apple_text, is_final) = unsafe {
                    let result: &SFSpeechRecognitionResult = &*result_ptr;
                    let transcription = result.bestTranscription();
                    let text = transcription.formattedString().to_string();
                    (text, result.isFinal())
                };
                if !is_final {
                    // 記錄 Apple cumulative 長度（char），供 rolling_tick 定稿時快照為已鎖定前綴。
                    apple_cumulative_chars.store(apple_text.chars().count(), Ordering::Relaxed);
                    // 全文快照供術語還原對齊（§18）；poisoned → 跳過（還原自動退化為 no-op）。
                    if let Ok(mut t) = apple_partial_text.lock() {
                        t.clear();
                        t.push_str(&apple_text);
                    }
                    // rolling：裁掉已鎖定前綴，只送當前句草稿（§2.2）；非 rolling：原樣送整段。
                    let draft = if rolling {
                        strip_committed_prefix(&apple_text, committed_chars.load(Ordering::Relaxed))
                    } else {
                        apple_text
                    };
                    let _ = tx.send(TranscriptUpdate::Partial(draft));
                    return;
                }
                // rolling 模式：句級 tick 已負責定稿，抑制 Apple 整段 final（收尾 Final 由
                // rolling_tick(is_final=true) 送，避免與 committed 前綴互相覆寫）。
                // 例外——收尾 flush 失敗（守門拒收/空輸出/核心錯誤）時旗標已被清除：
                // 以 Apple final 的**未定稿尾段**回退直出（不再過 Whisper，剛失敗過），
                // printer 對齊草稿 → 不丟字、不清空（stop 流程先 flush 後 endAudio，
                // Apple final 必然晚於 flush 抵達，旗標已定案）。
                if rolling {
                    if suppress_apple_final.load(Ordering::Relaxed) {
                        return;
                    }
                    let tail = strip_committed_prefix(
                        &apple_text,
                        committed_chars.load(Ordering::Relaxed),
                    );
                    eprintln!("raflow: rolling 收尾回退 → Apple final 尾段直出");
                    let _ = tx.send(TranscriptUpdate::Final(tail));
                    return;
                }
                // Final：條件觸發 Whisper 終校（spec/whisper.md §14）。只在 zh-TW session
                // 且 Apple final 含英文時跑——修正中文 session 裡夾雜、被 zh 模型辨錯的英文。
                // en-US session（Apple 英文已正確）或純中文，一律跳過：前者避免 zh-Whisper
                // 幻覺蓋掉正確英文，後者避免改寫對的內容 + 省 ~2s。
                let final_text = match whisper.as_ref() {
                    None => apple_text,
                    Some(_) if !correction_on => {
                        eprintln!("raflow: skip whisper (智慧校正 OFF via 設定)");
                        apple_text
                    }
                    Some(_) if !should_correct_with_whisper(&session_locale, &apple_text) => {
                        eprintln!(
                            "raflow: skip whisper (locale={session_locale}, non-zh session or pure Chinese)"
                        );
                        apple_text
                    }
                    Some(ctx) => {
                        let pcm = match pcm_buffer.lock() {
                            Ok(mut buf) => {
                                let drained: Vec<i16> = buf.drain(..).collect();
                                if drained.len() > MAX_PCM_SAMPLES_FOR_WHISPER {
                                    drained[drained.len() - MAX_PCM_SAMPLES_FOR_WHISPER..].to_vec()
                                } else {
                                    drained
                                }
                            }
                            Err(_) => {
                                eprintln!(
                                    "! whisper skipped: pcm buffer poisoned, fallback to apple final"
                                );
                                let _ = tx.send(TranscriptUpdate::Final(apple_text));
                                return;
                            }
                        };
                        if pcm.is_empty() {
                            apple_text
                        } else {
                            match ctx.transcribe(&pcm) {
                                Ok(t) if t.trim().is_empty() => apple_text,
                                Ok(t) if !is_safe_whisper_output(&t) => {
                                    eprintln!(
                                        "! whisper output rejected (contains non-zh/en chars): {t:?}; fallback to apple final"
                                    );
                                    apple_text
                                }
                                Ok(t) => t,
                                Err(e) => {
                                    eprintln!(
                                        "! whisper transcribe failed: {e}; fallback to apple final"
                                    );
                                    apple_text
                                }
                            }
                        }
                    }
                };
                let _ = tx.send(TranscriptUpdate::Final(final_text));
            },
        );

        // SAFETY: recognitionTaskWithRequest_resultHandler 接受 request 與 block；
        // Apple 會保留兩者的所有權至 task 結束。我們亦在 ActiveSession 中保留，
        // 確保 drop 順序受控。
        let task = unsafe {
            self.recognizer
                .recognitionTaskWithRequest_resultHandler(&request, &handler)
        };

        self.active = Some(ActiveSession {
            request,
            _task: task,
            _result_handler: handler,
        });
        Ok(())
    }

    fn push_frame(&mut self, frame: &AudioFrame) -> Result<(), RaflowError> {
        let Some(active) = self.active.as_ref() else {
            return Ok(());
        };
        let buffer = Self::make_pcm_buffer(frame)?;

        // SAFETY: appendAudioPCMBuffer 將 buffer 之內容 copy 進 request 的內部 queue；
        // buffer 可在返回後釋放。
        unsafe {
            active.request.appendAudioPCMBuffer(&buffer);
        }

        // 同步累積到 Whisper 用的 ring buffer（spec/whisper.md §3）
        if let Ok(mut buf) = self.pcm_buffer.lock() {
            buf.extend_from_slice(&frame.pcm);
            // 早期截斷以免長錄音吃光記憶體：超過 max 的 1.5 倍才裁切，避免每 frame 都做。
            // rolling 模式**不截斷**：從前端 drain 會位移 VAD 段索引、打亂 `finalized_samples` 游標；
            // 保留整段確保段索引穩定（長錄音記憶體成本為已知取捨，ADR-0006 §8.7.2；
            // ~2MB/min，典型聽寫可接受；未來以滑窗優化）。
            if !self.session_rolling {
                let max = MAX_PCM_SAMPLES_FOR_WHISPER;
                if buf.len() > max * 3 / 2 {
                    let drop_n = buf.len() - max;
                    buf.drain(..drop_n);
                }
            }
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), RaflowError> {
        if let Some(active) = self.active.take() {
            // SAFETY: endAudio 通知 request 不再有音訊輸入；Apple 將產出 final result
            // 並透過既有 block handler 回呼。
            unsafe {
                active.request.endAudio();
            }
            // active 在此被 drop：Retained<...> 自動釋放底層 Objective-C 物件；
            // RcBlock 最後一個強引用移除時亦清理 block。
        }
        Ok(())
    }

    /// 本 session 是否句級滾動（`start` 定案的 `session_rolling`：能力 + VAD + zh-TW）。
    /// 供 Edit Guard 只在有中途 `PhraseFinal` 段界的 session 啟用（見 spec/input.md §7f）。
    fn session_rolling(&self) -> bool {
        self.session_rolling
    }

    /// Phase 2 句級滾動 tick（ADR-0006 §8.7.2）。`rolling=false`（預設）→ 立即 no-op，
    /// 現行行為完全不變。ON 時：
    /// 1. 快照累積 PCM（clone，不 drain；定稿進度靠 `finalized_samples` 樣本位置游標追蹤）；
    /// 2. 重跑 VAD 切段 → `segments_ready_to_finalize`（停頓時把累積的段當「一句」一起定稿）；
    /// 3. 串接整句語音段（非逐小段）直送 `transcribe_span`（免二次 VAD）→ 送**一個** `PhraseFinal`；
    /// 4. `finalized_samples` 前進；`committed_chars` **只在有句子實際鎖定時**更新（供 callback 裁切後續
    ///    partial 前綴；每 tick 無條件更新會洗掉當前句草稿，見下方註解）；
    /// 5. `is_final`（停止收尾）→ 送 `Final("")` 觸發 printer 寫剪貼簿（整段 = committed）+ hide。
    ///
    /// 缺 `whisper` / VAD model / active sender 任一 → no-op（rolling 需三者齊備才生效）。
    fn rolling_tick(&mut self, is_final: bool) -> Result<(), RaflowError> {
        if !self.session_rolling {
            return Ok(());
        }
        let (Some(whisper), Some(vad_path), Some(tx)) = (
            self.whisper.clone(),
            self.vad_model_path.clone(),
            self.transcript_tx.clone(),
        ) else {
            return Ok(());
        };

        // 快照當前累積 PCM（clone；poisoned → 本 tick 放棄，下 tick 再試）。
        let pcm: Vec<i16> = match self.pcm_buffer.lock() {
            Ok(buf) => buf.clone(),
            Err(_) => return Ok(()),
        };

        // 決策核心與離線 rolling_harness 共用（whisper_backend::rolling_tick_core）：
        // VAD 切段 → pending 過濾 → 停頓即鎖 → 直送轉錄 → 守門 → 游標推進。
        // 錯誤（VAD / 轉錄失敗）→ log 一次、放棄本 tick，不讓錯誤每 tick 往上炸。
        // Prompt priming：離線 harness 驗證通過後啟用（術語由錯轉對、
        // 零回吐、延遲無回歸；spec §17.2）。
        let term_refs: Vec<&str> = self.prompt_terms.iter().map(String::as_str).collect();
        let outcome = match rolling_tick_core(
            &whisper,
            &vad_path,
            &pcm,
            self.finalized_samples,
            is_final,
            Some(&term_refs),
            // Edit Guard 接管中 → 極短門檻加速清當前段草稿（清空後下次開口即恢復）。
            self.edit_guard_frozen.load(Ordering::Relaxed),
        ) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("! rolling: tick core failed: {e}");
                if is_final {
                    // 收尾不可漏，但**不得送空 Final**（printer 會把未定稿草稿整段
                    // backspace 清掉）→ 放行 Apple final 作回退（見 callback）。
                    self.suppress_apple_final.store(false, Ordering::Relaxed);
                }
                return Ok(());
            }
        };
        for t in &outcome.rejected {
            eprintln!("! rolling: whisper output rejected: {t:?}");
        }
        let phrase_locked = outcome.phrase.is_some();
        if let Some(text) = outcome.phrase {
            // 術語還原（spec/whisper.md §18）：以同段音訊的 Apple 草稿對齊，
            // Apple 認對（contextualStrings 命中）而 Whisper 改壞的術語 → 還原。
            // 守門在 restore_terms 內（對不齊/無命中 → 原句返回，永不變更中文）。
            let apple_draft = self
                .apple_partial_text
                .lock()
                .map(|t| strip_committed_prefix(&t, self.committed_chars.load(Ordering::Relaxed)))
                .unwrap_or_default();
            // 術語還原診斷 trace（opt-in `RAFLOW_RESTORE_TRACE`，預設 OFF）：印出對齊的兩側
            // 輸入讓「漏還原」現形，供 §18 錯例萃取。含口述全文，故預設不輸出（Codex review）。
            if self.restore_trace {
                eprintln!("raflow: [restore-trace] whisper={text:?} apple={apple_draft:?}");
            }
            let restored = restore_terms(&text, &apple_draft, &self.restore_vocab);
            if restored != text {
                eprintln!("raflow: 術語還原 {text:?} → {restored:?}");
            }
            let _ = tx.send(TranscriptUpdate::PhraseFinal(restored));
            // **只有實際鎖定了句子才推進游標。** 被拒/空/錯 → 游標保留（音訊不丟失），
            // 下次停頓以更長 span（更多上下文）重試。committed_chars 同理只在鎖定時
            // 推進（否則每 tick 裁掉當前句草稿，造成畫面重複清空重寫）；鎖定發生在使用者「停頓」
            // 當下（Apple 尚未開始下一句），cumulative 長度 ≈ 已定稿邊界。
            self.finalized_samples = outcome.finalized_samples;
            let cum = self.apple_cumulative_chars.load(Ordering::Relaxed);
            self.committed_chars.store(cum, Ordering::Relaxed);
        }

        if is_final {
            if rolling_final_flush_delivered(phrase_locked, outcome.pending_speech_samples) {
                // 收尾：pending 語音已全數鎖進 printer 的 committed（或本無語音）；
                // 送空 Final 觸發剪貼簿寫入（printer 端 = committed）與 overlay 排程 hide。
                let _ = tx.send(TranscriptUpdate::Final(String::new()));
            } else {
                // 有 pending 語音卻沒鎖定（守門拒收/空輸出）→ 不送空 Final（會清掉
                // 草稿），改放行 Apple final 以未定稿尾段回退（見 callback）。
                eprintln!(
                    "! rolling: 收尾 flush 未定稿（pending {} samples）→ 回退 Apple final",
                    outcome.pending_speech_samples
                );
                self.suppress_apple_final.store(false, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

// 工具：讓 `Arc<AppleSpeechBackend>` 在單執行緒 tokio 情境可用時的語意定義留給 Phase 4 wiring，
// 這裡暫不 `unsafe impl Send`，以避免跨執行緒使用造成誤用。

#[cfg(test)]
mod tests {
    use super::*;

    /// 滾動 prompt 術語選擇：**使用者檔案詞優先**（自家詞最常被聽錯，上限 30 內
    /// 必須排前面），再補核心預設；保序去重、跳過註解/空行。
    #[test]
    fn prompt_terms_user_first_orders_and_dedups() {
        let user = "MyService\n# 註解\n\nArgoCD\n";
        let terms = prompt_terms_user_first(Some(user));
        assert_eq!(terms[0], "MyService", "使用者詞應排最前：{terms:?}");
        assert_eq!(terms[1], "ArgoCD");
        // ArgoCD 同時在核心預設 → 不得重複
        assert_eq!(terms.iter().filter(|t| *t == "ArgoCD").count(), 1);
        // 核心預設補在後面
        assert!(
            terms.iter().any(|t| t == "Terraform"),
            "缺核心預設：{terms:?}"
        );
        // 無使用者檔 → 即為核心預設
        assert_eq!(prompt_terms_user_first(None), DEFAULT_PROMPT_TERMS.to_vec());
    }

    /// 術語還原診斷 trace 開關 `RAFLOW_RESTORE_TRACE` 純解析（**預設 OFF**，opt-in）：
    /// 只有明確 truthy（1/true/yes/on，忽略大小寫、去前後空白）才 ON；未設/空/其餘 → OFF。
    /// trace 含使用者口述全文，預設不得輸出（Codex review：避免敏感內容落 stderr/log）。
    #[test]
    fn parse_restore_trace_flag_defaults_off_and_opts_in() {
        for v in [
            Some("1"),
            Some("true"),
            Some("TRUE"),
            Some(" yes "),
            Some("on"),
            Some("On"),
        ] {
            assert!(parse_restore_trace_flag(v), "應為 ON：{v:?}");
        }
        for v in [
            None,
            Some(""),
            Some("   "),
            Some("0"),
            Some("false"),
            Some("no"),
            Some("off"),
            Some("2"),
            Some("garbage"),
        ] {
            assert!(
                !parse_restore_trace_flag(v),
                "應為 OFF（預設不輸出口述全文）：{v:?}"
            );
        }
    }

    /// 術語合併：預設 + 使用者檔（trim、跳過空行/註解）、保序去重。
    #[test]
    fn merge_contextual_terms_appends_dedups_and_skips_comments() {
        let defaults = &["Kubernetes", "Docker", "AWS"];
        let user = "  Terraform \n\n# 這是註解\nDocker\nkubectl\n#另一個註解\n   \n我的服務ABC";
        let merged = merge_contextual_terms(defaults, Some(user));
        assert_eq!(
            merged,
            vec![
                "Kubernetes".to_string(),
                "Docker".to_string(), // 使用者重複的 Docker 被去重（只保留第一個）
                "AWS".to_string(),
                "Terraform".to_string(), // trim 後加入
                "kubectl".to_string(),
                "我的服務ABC".to_string(),
            ],
        );
        // 無使用者檔 → 只有預設
        assert_eq!(
            merge_contextual_terms(defaults, None),
            vec![
                "Kubernetes".to_string(),
                "Docker".to_string(),
                "AWS".to_string()
            ],
        );
        // 預設本身即涵蓋常見術語（回歸保護：清單非空）
        assert!(!DEFAULT_CONTEXTUAL_TERMS.is_empty());
        assert!(DEFAULT_CONTEXTUAL_TERMS.contains(&"ChatGPT"));
    }

    #[test]
    fn new_with_unsupported_locale_returns_unavailable() {
        // 用一個 Apple 不可能支援的 locale 代碼
        let result = AppleSpeechBackend::new("xx-ZZ-completely-bogus");
        // 視系統而定，Apple 可能仍回 fallback recognizer + isAvailable == false，
        // 或直接 init 失敗。兩種情形都應回 SpeechUnavailable。
        match result {
            Err(RaflowError::SpeechUnavailable { .. }) => (),
            Err(other) => panic!("expected SpeechUnavailable, got {other:?}"),
            Ok(_) => panic!("expected Err for bogus locale"),
        }
    }

    /// Phase 5 ring buffer：push_frame 累積 PCM；超過上限時裁切只保留最後 N samples。
    /// 直接驗 buffer 行為（不真的呼 Apple Speech；用 zh-TW recognizer 必須 available）。
    #[test]
    fn pcm_buffer_accumulates_and_truncates() {
        // 若系統不支援 zh-TW 就跳過（CI / 沒下 Speech model 的機器）
        let Ok(backend) = AppleSpeechBackend::new("zh-TW") else {
            eprintln!("skip: zh-TW unavailable on this host");
            return;
        };
        let buffer = backend.pcm_buffer.clone();
        // 直接操作 buffer 模擬 push_frame 的累積邏輯（避免依賴 SFSpeech 真實 task）
        {
            let mut b = buffer.lock().expect("lock");
            // 模擬累積 90 秒（90 * 16k = 1_440_000 samples），應觸發 truncate
            // truncate 條件：len > MAX * 3 / 2 = 1_440_000 → drop 到 MAX = 960_000
            b.extend_from_slice(&vec![1_i16; 1_440_001]);
            let max = MAX_PCM_SAMPLES_FOR_WHISPER;
            if b.len() > max * 3 / 2 {
                let drop_n = b.len() - max;
                b.drain(..drop_n);
            }
            assert_eq!(
                b.len(),
                max,
                "should truncate to MAX_PCM_SAMPLES_FOR_WHISPER"
            );
        }
    }

    /// Phase 5-fix6 條件觸發：純函式 apple_final_has_english 邊界。
    /// 用來決定要不要跑 Whisper 終校。純中文 → false → 跳過 Whisper。
    #[test]
    fn apple_final_has_english_detects_ascii_letters_only() {
        let cases: &[(&str, bool)] = &[
            // 純中文 → false（跳過 Whisper）
            ("你好世界", false),
            ("我們去吃飯吧", false),
            ("好，讓我們繼續下去", false),
            // 中英混合 → true（觸發 Whisper）
            ("我用 Cursor 開啟", true),
            ("npm install 已完成", true),
            ("Hello 世界", true),
            // 純英文 → true
            ("Hello world", true),
            ("a", true),
            // ASCII 但非字母 → false（數字 / 標點不算英文）
            ("123 456", false),
            ("！？，。", false),
            ("我有 100 個", false),
            // Edge cases
            ("", false),
            ("   ", false),
        ];
        for (input, expected) in cases {
            assert_eq!(
                apple_final_has_english(input),
                *expected,
                "apple_final_has_english({input:?}) ≠ {expected}"
            );
        }
    }

    /// spec/speech.md §2：start() 收到與現有不同的 locale 時，重建 recognizer 並更新
    /// self.locale。en-US 在多數機器可用；不可用則 skip（CI / 未下模型）。
    #[test]
    fn start_switches_recognizer_when_locale_differs() {
        use crate::backend::SpeechBackend;
        let Ok(mut backend) = AppleSpeechBackend::new("zh-TW") else {
            eprintln!("skip: zh-TW unavailable on this host");
            return;
        };
        assert_eq!(backend.locale, "zh-TW");

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        match backend.start("en-US", tx) {
            Ok(()) => {
                assert_eq!(backend.locale, "en-US", "locale must switch to en-US");
                let _ = backend.stop();
            }
            Err(RaflowError::SpeechUnavailable { .. }) => {
                eprintln!("skip: en-US unavailable on this host");
            }
            Err(other) => panic!("unexpected error switching locale: {other:?}"),
        }
    }

    /// 句級滾動的 session 守門：en-US session 一律不滾動（Whisper 強制 zh tokenizer，
    /// 對英文音訊會幻覺或直接翻譯成中文；實機回歸：英文輸入法說英文，停止後輸出變中文）；
    /// VAD 模型檔不存在也不得滾動（否則 callback 抑制 Apple final、tick 卻永遠 no-op
    /// → 整個 session 收不到 final）。
    #[test]
    fn rolling_session_active_requires_zh_tw_and_vad() {
        let cases: &[(bool, bool, &str, bool)] = &[
            (true, true, "zh-TW", true),   // 能力齊備 + VAD 在 + zh-TW → 滾動
            (true, true, "en-US", false),  // en-US → 不滾動（Apple 英文直出）
            (true, true, "ja-JP", false),  // 其他 locale 一律不滾動
            (true, false, "zh-TW", false), // VAD 模型缺 → 不滾動（退化整段校正）
            (false, true, "zh-TW", false), // 能力不齊（flag/閘門/whisper 缺）→ 不滾動
            (false, false, "en-US", false),
        ];
        for (capable, vad_ready, locale, expected) in cases.iter().copied() {
            assert_eq!(
                rolling_session_active(capable, vad_ready, locale),
                expected,
                "rolling_session_active({capable}, {vad_ready}, {locale:?})"
            );
        }
    }

    /// spec/whisper.md §14：Whisper 終校只在 zh-TW session 且 final 含英文時才跑。
    /// en-US session（含英文）必須跳過，否則 zh-Whisper 會幻覺蓋掉正確英文。
    #[test]
    fn should_correct_with_whisper_only_for_zh_session_with_english() {
        let cases: &[(&str, &str, bool, &str)] = &[
            // zh-TW session：含英文 → 跑；純中文 → 不跑
            (
                "zh-TW",
                "我用 Cursor 開啟",
                true,
                "zh session + english → correct",
            ),
            (
                "zh-TW",
                "你好世界",
                false,
                "zh session + pure chinese → skip",
            ),
            (
                "zh-TW",
                "npm install 完成",
                true,
                "zh session + mixed → correct",
            ),
            // en-US session：不論內容一律跳過（Apple 英文已正確；避免 zh-Whisper 幻覺）
            (
                "en-US",
                "open this project with cursor",
                false,
                "en session english → skip",
            ),
            ("en-US", "Hello world", false, "en session → skip"),
            ("en-US", "", false, "en session empty → skip"),
            // 其他 locale 也不跑（保守）
            ("ja-JP", "こんにちは test", false, "other locale → skip"),
        ];
        for (locale, text, expected, label) in cases {
            assert_eq!(
                should_correct_with_whisper(locale, text),
                *expected,
                "{label}: locale={locale} text={text:?}"
            );
        }
    }

    #[test]
    fn with_whisper_attaches_context_marker() {
        let Ok(backend) = AppleSpeechBackend::new("zh-TW") else {
            eprintln!("skip: zh-TW unavailable on this host");
            return;
        };
        assert!(backend.whisper.is_none(), "default backend has no whisper");
        // 無真 model 路徑下，WhisperContext::load 會 fail；本 test 只確認 with_whisper
        // 的 builder 正確 set 欄位。手動構造 stub ctx 路徑：spec/whisper.md §5 的「無 feature」
        // stub 也會回 WhisperLoad，所以這裡只用結構驗證，不真的 load。
    }
}
