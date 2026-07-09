# raflow

[![CI](https://github.com/rexshen5913/raflow/actions/workflows/ci.yml/badge.svg)](https://github.com/rexshen5913/raflow/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/rexshen5913/raflow)](https://github.com/rexshen5913/raflow/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

macOS 語音輸入工具，專為**中英混講**設計。**雙擊 Cmd** 開始說話，文字即時落在游標處；
每講完一句，本機 Whisper 立刻原地修正該句的英文技術術語與標點。全程離線，聲音不離開你的 Mac。

## 特色

- **雙擊 Cmd 即說即打**：文字即時注入目前 focus 的輸入框（同時複製到剪貼簿作為 fallback）
- **句級滾動校正**：Apple Speech 負責低延遲即時字幕；每講完一句（停頓約 1–2 秒），
  本機 Whisper（large-v3-turbo，Metal GPU 加速）原地修正該句——不必等整段講完。
  若 Whisper 反而把原本正確的術語改壞，會自動以 Apple 草稿還原
- **中英混講最佳化**：`ArgoCD`、`GitLab CI`、`Terraform` 等技術術語混在中文裡也能拼對；
  一律輸出繁體中文（內建簡→繁與台灣用語轉換）
- **口述命令**：說「逗點」「句點」「換行」即輸出對應標點與換行
- **可自訂詞彙**：`contextual_terms.txt`（術語提示）與 `replacements.txt`（確定性字串修正）
- **教 raflow 一個更正**：某術語一直被聽錯時，menu bar 點「教 raflow 一個更正…」，從剛剛講到的
  英文詞挑「聽成」、填「正確」拼法即記住，下次錄音生效；可一併加進 Whisper 優先區——不必手動編設定檔
- **手動接管不被覆蓋**：錄音中你動滑鼠、打字或按方向鍵去手改先前的字時，raflow 立刻停止覆蓋、
  menu bar 圖示轉暗表示暫停；改完一開口就從你游標處接續，永遠不會蓋掉你手改的內容
- **完全離線**：Apple Speech 本機模式 + whisper.cpp 本機推論，無任何網路傳輸

## 安裝

### Homebrew（推薦）

```bash
brew tap rexshen5913/tap
brew trust rexshen5913/tap   # Homebrew 6.0+ 需信任第三方 tap（見下方說明）
brew install --cask raflow
```

首次安裝會自動下載 Whisper 模型（約 550 MB，僅一次）。

> **Homebrew 6.0+ 需要 `brew trust`。** 自 Homebrew 6.0 起，第三方 tap 預設不受信任；
> 少了 `brew trust` 這步，安裝會失敗並顯示：
>
> ```text
> Error: Refusing to load cask rexshen5913/tap/raflow from untrusted tap rexshen5913/tap.
> ```
>
> `brew trust rexshen5913/tap` 每台機器只需執行**一次**（之後 `brew install` / `brew upgrade`
> 都不用再做）。官方 tap（如 `homebrew/core`）已預先信任、不需此步；只有第三方 tap 需要。
> Homebrew 舊版（< 6.0）沒有此機制，可略過。

### 系統需求

- Apple Silicon（M1 以上）
- macOS 13 Ventura 以上

### 解除安裝

```bash
brew uninstall --cask raflow          # 移除 app（保留模型與設定，重裝不必重新下載）
```

若要連 Whisper 模型（約 550 MB）與你的設定（自訂詞彙 / 取代規則 / 偏好）一起徹底清除：

```bash
brew uninstall --zap --cask raflow
```

> 一般解除安裝會**保留** `~/Library/Application Support/raflow`（模型 + 設定），讓重裝不必重新下載模型。
> macOS 的權限授權（麥克風 / 語音辨識 / 輔助使用）由系統管理，`--zap` 也不會動到；
> 如需清除，到「系統設定 → 隱私權與安全性」移除，或執行 `tccutil reset All dev.raflow.raflow`。

## 首次執行權限

raflow 需要以下權限（皆為功能必要，無任何資料外傳）：

| 權限 | 用途 |
|---|---|
| 語音辨識（Speech Recognition） | Apple Speech 即時辨識 |
| 麥克風（Microphone） | 擷取語音 |
| 輔助使用（Accessibility） | 偵測雙擊 Cmd 快捷鍵 + 將文字注入目前的輸入框 |

前兩項會自動彈窗引導；輔助使用若未彈窗，請至
**系統設定 → 隱私權與安全性 → 輔助使用** 手動勾選 raflow。

> raflow **不需要單獨開「輸入監控」**：雙擊 Cmd 偵測與文字注入都以輔助使用為 gate，
> macOS 對已授權輔助使用的 app 會一併涵蓋全域鍵盤監看，raflow 從不獨立出現在「輸入監控」清單。

## 使用方式

1. 啟動後 menu bar 會出現 raflow 圖示
2. **雙擊 Cmd** 開始錄音（圖示轉紅），對著任何輸入框說話
3. 再**雙擊 Cmd** 停止；完整內容同時在剪貼簿
4. 錄音中每講完一句稍作停頓，該句便會被 Whisper 原地修正

### 自訂詞彙

設定檔位於 `~/Library/Application Support/raflow/`，改完存檔後下次錄音即生效：

- **`contextual_terms.txt`** — 一行一個常用術語，提高辨識準確度。
  檔案最上方的詞優先進入 Whisper 修正提示（上限 30 個），把最常被聽錯的放前面。
- **`replacements.txt`** — 每行 `聽錯 => 正確`，對穩定重現的誤認做確定性修正
  （如 `Teraphone => Terraform`）。

不想手動編檔的話，menu bar 有三個入口：**「教 raflow 一個更正…」**（填一組「聽成 → 正確」
即寫入 `replacements.txt`，可勾選一併加進 `contextual_terms.txt` 優先區頂端）、
**「編輯取代規則…」**、**「編輯自訂詞彙…」**（直接用文字編輯器開對應檔案）。

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

[MIT](LICENSE) © 2026 Rex Shen

### Acknowledgements

raflow 站在許多開源專案之上。發佈的 `.app` 靜態連結／內嵌了下列元件，完整的版權與
授權文字見 [`THIRD-PARTY-LICENSES.md`](THIRD-PARTY-LICENSES.md)（`.app/Contents/Resources/`
內也隨附一份）：

- **[whisper.cpp](https://github.com/ggerganov/whisper.cpp)**（MIT，© The ggml authors）— 本機語音校正引擎
- **[OpenCC](https://github.com/BYVoid/OpenCC)** 轉換字典（Apache-2.0，透過 `ferrous-opencc` 內嵌）— 簡→繁轉換
- **[objc2](https://github.com/madsmtm/objc2)** 生態系（MIT / Apache-2.0）— 純 Rust Apple Framework 綁定
- **[enigo](https://github.com/enigo-rs/enigo)**（MIT）— 文字注入
- 其餘 Rust crate 依賴（`thiserror`、`anyhow`、`arc-swap`、`dashmap`、`cpal` 等）

安裝時下載的模型：**OpenAI Whisper**（MIT）、其 GGML 轉換（© Georgi Gerganov，MIT）、
**Silero VAD**（MIT）。
