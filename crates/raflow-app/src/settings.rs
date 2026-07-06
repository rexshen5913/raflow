//! 使用者設定（單一來源：docs/spec/settings.md）。
//!
//! `settings.json` 載入/儲存 + `RAFLOW_ROLLING` env 覆寫摺疊。載入時機為
//! 「啟動一次 + menu 切換即時更新 ArcSwap」；直接手改檔案需重啟才生效
//! （不做 file watch——簡單性優先）。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

/// 使用者可調行為開關（serde 定義為唯一來源；欄位缺漏補預設、未知欄位忽略，
/// 向前相容）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// ON：錄音開始時依鍵盤輸入法選 zh-TW / en-US（ADR-0007）；OFF：固定 zh-TW。
    #[serde(default = "default_true")]
    pub auto_locale: bool,
    /// ON：句級滾動 + 停止時整段 Whisper 校正；OFF：完全所見即所得（Whisper 不介入）。
    #[serde(default = "default_true")]
    pub whisper_correction: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_locale: true,
            whisper_correction: true,
        }
    }
}

impl Settings {
    /// 解析 settings.json 內容；任何解析失敗 → 預設值（容錯，spec §2）。
    pub fn from_json_str(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    /// 序列化為 pretty JSON。純資料 struct 序列化不會失敗；防禦性回空物件
    /// （下次 load 會補預設）。
    pub fn to_json_string(self) -> String {
        serde_json::to_string_pretty(&self).unwrap_or_else(|_| "{}".to_string())
    }

    /// `RAFLOW_ROLLING` env 覆寫摺疊（spec §3）：已設 → 解析值蓋過
    /// `whisper_correction`（debug 用，雙向覆寫）；未設 → 維持原值。
    pub fn apply_env_override(mut self, rolling_env: Option<&str>) -> Self {
        if let Some(v) = rolling_env {
            self.whisper_correction = raflow_speech::parse_rolling_flag(Some(v));
        }
        self
    }
}

/// 設定檔路徑：env `RAFLOW_SETTINGS` 覆寫（測試/debug 用），否則
/// `$HOME/Library/Application Support/raflow/settings.json`。
/// 薄層 env I/O，不另測（比照 `rolling_enabled`）。
pub fn settings_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("RAFLOW_SETTINGS") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("raflow");
    p.push("settings.json");
    Some(p)
}

/// 載入設定：檔案不存在 / 讀取失敗 / JSON 損壞 → 一律預設值。
pub fn load(path: &Path) -> Settings {
    match std::fs::read_to_string(path) {
        Ok(s) => Settings::from_json_str(&s),
        Err(_) => Settings::default(),
    }
}

/// 儲存設定：先寫 `.tmp` 再 rename（避免寫一半損壞）。失敗由呼叫端記 log，
/// 不阻擋 in-memory 生效。
pub fn save(path: &Path, settings: Settings) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, settings.to_json_string())?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 解析容錯 + 欄位缺漏補預設 + 未知欄位忽略（spec §2）。
    #[test]
    fn from_json_str_tolerates_and_defaults() {
        let cases: &[(&str, (bool, bool))] = &[
            ("", (true, true)),                                 // 空 → 預設
            ("not json at all", (true, true)),                  // 損壞 → 預設
            ("{}", (true, true)),                               // 空物件 → 全預設
            (r#"{"auto_locale":false}"#, (false, true)),        // 缺漏補預設
            (r#"{"whisper_correction":false}"#, (true, false)), // 缺漏補預設
            (
                r#"{"auto_locale":false,"whisper_correction":false}"#,
                (false, false),
            ),
            (r#"{"auto_locale":false,"future_field":123}"#, (false, true)), // 未知欄位忽略
        ];
        for (input, (auto, wc)) in cases.iter().copied() {
            let s = Settings::from_json_str(input);
            assert_eq!(
                (s.auto_locale, s.whisper_correction),
                (auto, wc),
                "from_json_str({input:?})"
            );
        }
    }

    /// 序列化 round-trip：寫出再讀回必須相等。
    #[test]
    fn json_round_trip_preserves_settings() {
        let cases = [
            Settings {
                auto_locale: true,
                whisper_correction: true,
            },
            Settings {
                auto_locale: false,
                whisper_correction: true,
            },
            Settings {
                auto_locale: true,
                whisper_correction: false,
            },
            Settings {
                auto_locale: false,
                whisper_correction: false,
            },
        ];
        for original in cases {
            let json = original.to_json_string();
            assert_eq!(
                Settings::from_json_str(&json),
                original,
                "round-trip {json}"
            );
        }
    }

    /// env 覆寫摺疊（spec §3）：已設 → 依 parse_rolling_flag 語意雙向覆寫；未設 → 不動。
    #[test]
    fn env_override_folds_into_whisper_correction() {
        // (env, file_wc, expected_wc)
        let cases: &[(Option<&str>, bool, bool)] = &[
            (None, true, true),       // 未設 → 維持檔案值
            (None, false, false),     // 未設 → 維持檔案值
            (Some("0"), true, false), // =0 → 強制 OFF
            (Some("false"), true, false),
            (Some(""), true, false),  // 空值 = OFF（同 parse_rolling_flag）
            (Some("1"), false, true), // =1 → 強制 ON（蓋過檔案 OFF）
        ];
        for (env, file_wc, expected) in cases.iter().copied() {
            let s = Settings {
                auto_locale: true,
                whisper_correction: file_wc,
            }
            .apply_env_override(env);
            assert_eq!(s.whisper_correction, expected, "env={env:?} file={file_wc}");
            assert!(s.auto_locale, "env 覆寫不得影響 auto_locale");
        }
    }

    /// load/save 檔案 I/O：save → load round-trip；不存在 → 預設。
    #[test]
    fn load_save_round_trip_and_missing_file_defaults() {
        let dir = std::env::temp_dir().join(format!("raflow-settings-test-{}", std::process::id()));
        let path = dir.join("settings.json");
        let _ = std::fs::remove_file(&path);

        // 不存在 → 預設
        assert_eq!(load(&path), Settings::default(), "missing file → default");

        // save（含自動建目錄）→ load round-trip
        let s = Settings {
            auto_locale: false,
            whisper_correction: false,
        };
        save(&path, s).unwrap();
        assert_eq!(load(&path), s, "round-trip via file");
        // 無 .tmp 殘留
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file must be renamed away"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
