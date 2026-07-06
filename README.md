# raflow

[![CI](https://github.com/rexshen5913/raflow/actions/workflows/ci.yml/badge.svg)](https://github.com/rexshen5913/raflow/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rexshen5913/raflow)](https://github.com/rexshen5913/raflow/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

macOS 語音輸入工具，專為**中英混講**設計。**雙擊 Cmd** 開始說話，文字即時落在游標處；
每講完一句，本機 Whisper 立刻原地修正該句的英文技術術語與標點。全程離線，聲音不離開你的 Mac。

## 特色

- **雙擊 Cmd 即說即打**：文字即時注入目前 focus 的輸入框（同時複製到剪貼簿作為 fallback）
- **句級滾動校正**：Apple Speech 負責低延遲即時字幕；每講完一句（停頓約 1–2 秒），
  本機 Whisper（large-v3-turbo，Metal GPU 加速）原地修正該句——不必等整段講完
- **中英混講最佳化**：`ArgoCD`、`GitLab CI`、`Terraform` 等技術術語混在中文裡也能拼對；
  一律輸出繁體中文（內建簡→繁與台灣用語轉換）
- **口述命令**：說「逗點」「句點」「換行」即輸出對應標點與換行
- **可自訂詞彙**：`contextual_terms.txt`（術語提示）與 `replacements.txt`（確定性字串修正）
- **完全離線**：Apple Speech 本機模式 + whisper.cpp 本機推論，無任何網路傳輸

## 安裝

### Homebrew（推薦）

```bash
brew tap rexshen5913/tap
brew install --cask raflow
```

首次安裝會自動下載 Whisper 模型（約 550 MB，僅一次）。

### 系統需求

- Apple Silicon（M1 以上）
- macOS 13 Ventura 以上

## 首次執行權限

raflow 需要以下權限（皆為功能必要，無任何資料外傳）：

| 權限 | 用途 |
|---|---|
| 語音辨識（Speech Recognition） | Apple Speech 即時辨識 |
| 麥克風（Microphone） | 擷取語音 |
| 輸入監控（Input Monitoring） | 偵測雙擊 Cmd 快捷鍵 |
| 輔助使用（Accessibility） | 將文字注入目前的輸入框 |

前兩項會自動彈窗引導；後兩項若未彈窗，請至
**系統設定 → 隱私權與安全性** 手動勾選 raflow。

## 使用方式

1. 啟動後 menu bar 會出現 raflow 圖示
2. **雙擊 Cmd** 開始錄音（圖示轉紅），對著任何輸入框說話
3. 再**雙擊 Cmd** 停止；完整內容同時在剪貼簿
4. 錄音中每講完一句稍作停頓，該句便會被 Whisper 原地修正

### 自訂詞彙

設定檔位於 `~/Library/Application Support/raflow/`，改完存檔後下次錄音即生效：

- **`contextual_terms.txt`** — 一行一個常用術語，提高辨識準確度。
  檔案最上方的詞優先進入 Whisper 修正提示（上限 20 個），把最常被聽錯的放前面。
- **`replacements.txt`** — 每行 `聽錯 => 正確`，對穩定重現的誤認做確定性修正
  （如 `Teraphone => Terraform`）。

## 從原始碼建置

```bash
git clone https://github.com/rexshen5913/raflow.git
cd raflow

make test                 # 跑 workspace 全部測試
make whisper-model-turbo  # 下載 Whisper 模型（~547 MB）
make whisper-vad-model    # 下載 Silero VAD 模型（~1 MB）
make install-app-whisper  # 建置 .app 並安裝到 /Applications
```

其他常用 target：`make check`（cargo check）、`make lint`（clippy，warning 視為 error）、
`make fmt`（rustfmt）。

## 架構

純 Rust workspace（Rust 2024 edition），macOS 原生 UI（menu bar + NSPanel 浮動視窗）：

| Crate | 職責 |
|---|---|
| `raflow-hotkey` | 雙擊 Cmd 偵測（NSEvent global monitor） |
| `raflow-audio` | 麥克風 PCM 擷取（cpal） |
| `raflow-speech` | Apple Speech 串流（objc2 純 Rust 綁定）+ whisper.cpp 句級滾動校正 |
| `raflow-input` | 文字注入（CGEvent）、取代規則、串流 diff |
| `raflow-app` | 協調層：狀態機、menu bar、浮動字幕視窗 |
| `raflow-core` | 共用型別與錯誤定義 |

模組間以 mpsc channel 通訊；錯誤處理全面採 `thiserror`/`anyhow`，不使用 `unwrap`。

## 隱私

- 語音辨識與校正全部在本機執行（Apple Speech 本機模式 + whisper.cpp Metal）
- 不收集任何遙測、不發出任何網路請求（模型檔僅於安裝時下載一次）

## License

[MIT](LICENSE)
