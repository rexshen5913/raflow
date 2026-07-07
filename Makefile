.PHONY: test test-integration check fmt lint icons dev-cert bundle bundle-whisper run run-bundle install-app install-app-whisper whisper-model-small whisper-model-turbo whisper-vad-model vad-harness tts-fixtures rolling-harness clean

BUNDLE_DIR := target/release/raflow.app
BUNDLE_BIN := $(BUNDLE_DIR)/Contents/MacOS/raflow
BUNDLE_PLIST := $(BUNDLE_DIR)/Contents/Info.plist
# bundle 版本一律取自 workspace Cargo.toml（避免 Info.plist 版本與 crate 版本漂移）
VERSION := $(shell grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
BUNDLE_RES := $(BUNDLE_DIR)/Contents/Resources
# self-signed dev cert：所有 rebuild 用同一顆 cert 才能讓 macOS TCC 記住權限
DEV_CERT_NAME := raflow-dev
ICONS_DIR := packaging/icons
APP_ICON := $(ICONS_DIR)/icon.icns
ICONSET := $(ICONS_DIR)/icon.iconset
APP_ICON_SIZES := 16 32 64 128 256 512 1024
APP_ICON_RETINA_SIZES := 16 32 128 256 512

# Phase 5：Whisper model 路徑（spec/whisper.md §4）
WHISPER_MODEL_DIR := $(HOME)/Library/Application Support/raflow/models
WHISPER_MODEL_SMALL := $(WHISPER_MODEL_DIR)/ggml-small.bin
WHISPER_MODEL_SMALL_URL := https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin
WHISPER_COREML_SMALL := $(WHISPER_MODEL_DIR)/ggml-small-encoder.mlmodelc
WHISPER_COREML_SMALL_URL := https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-encoder.mlmodelc.zip
# large-v3-turbo q5_0（~547 MB，Metal GPU；術語辨識比 small 準）
WHISPER_MODEL_TURBO := $(WHISPER_MODEL_DIR)/ggml-large-v3-turbo-q5_0.bin
WHISPER_MODEL_TURBO_URL := https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin

# Phase 1 抗幻覺串流：Silero VAD 模型（~1 MB，檔名與來源沿用 whisper.cpp 官方發佈）
WHISPER_VAD_MODEL := $(WHISPER_MODEL_DIR)/ggml-silero-v6.2.0.bin
WHISPER_VAD_MODEL_URL := https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin

# 預設測試：跳過標註 #[ignore] 的整合測試
test:
	cargo test --workspace

test-integration:
	cargo test --workspace -- --ignored

check:
	cargo check --workspace --all-targets

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

# icons：從 SVG 產出 .icns（app icon）與 menu bar 用 PNG（@1x、@2x）
# 需要 brew install librsvg 提供 rsvg-convert
icons:
	rm -rf "$(ICONSET)"
	mkdir -p "$(ICONSET)"
	@for s in $(APP_ICON_SIZES); do \
		rsvg-convert -w $$s -h $$s $(ICONS_DIR)/icon.svg -o $(ICONSET)/icon_$${s}x$${s}.png; \
	done
	@for s in $(APP_ICON_RETINA_SIZES); do \
		r=$$((s*2)); \
		rsvg-convert -w $$r -h $$r $(ICONS_DIR)/icon.svg -o $(ICONSET)/icon_$${s}x$${s}@2x.png; \
	done
	iconutil -c icns "$(ICONSET)" -o "$(APP_ICON)"
	rsvg-convert -w 22 -h 22 $(ICONS_DIR)/menubar-idle.svg      -o $(ICONS_DIR)/menubar-idle.png
	rsvg-convert -w 44 -h 44 $(ICONS_DIR)/menubar-idle.svg      -o $(ICONS_DIR)/menubar-idle@2x.png
	rsvg-convert -w 22 -h 22 $(ICONS_DIR)/menubar-recording.svg -o $(ICONS_DIR)/menubar-recording.png
	rsvg-convert -w 44 -h 44 $(ICONS_DIR)/menubar-recording.svg -o $(ICONS_DIR)/menubar-recording@2x.png
	rsvg-convert -w 22 -h 22 $(ICONS_DIR)/menubar-frozen.svg    -o $(ICONS_DIR)/menubar-frozen.png
	rsvg-convert -w 44 -h 44 $(ICONS_DIR)/menubar-frozen.svg    -o $(ICONS_DIR)/menubar-frozen@2x.png

# 建立 self-signed Code Signing 憑證（idempotent；已存在會跳過）。
# 所有 bundle target 依賴此 target，保證 codesign 用得到 raflow-dev cert。
dev-cert:
	@bash packaging/create-dev-cert.sh "$(DEV_CERT_NAME)"

# 重新彙整第三方授權聲明（THIRD-PARTY-LICENSES.md）。發佈 binary 靜態連結/內嵌了
# whisper.cpp、OpenCC、objc2、enigo 等 MIT/Apache 專案，須保留其版權/授權文字。
# 需 cargo-about：cargo install cargo-about --features cli。about.toml/about.hbs 為設定。
# 產出會被 bundle / bundle-whisper 複製進 .app/Contents/Resources。
#
# 兩段組成、**確定性**產生（不依賴人工記憶）：
#   (1) cargo-about 產出 crate 授權 → THIRD-PARTY-LICENSES.md（覆寫）
#   (2) 附加 about-native.md：whisper.cpp / OpenCC 字典等**非 crate 內嵌來源**的手動段落
#       （about-native.md 為此段的唯一真實來源，已納入版控；cargo-about 不會動它）。
# 少了 (2) 會漏掉內嵌 native 元件的必要 attribution，故 concat 寫進同一 target。
licenses:
	cargo about generate about.hbs --all-features -o THIRD-PARTY-LICENSES.md
	printf '\n' >> THIRD-PARTY-LICENSES.md
	cat about-native.md >> THIRD-PARTY-LICENSES.md

# 打包 .app bundle；Resources/ 放 app icon
# 用同一顆 self-signed dev cert 簽章，避免每次 rebuild 重置 TCC 權限授權。
# 真正穩定的代碼簽章（Developer ID + notarization）屬後續 Phase。
bundle: icons dev-cert
	cargo build --release -p raflow-app
	rm -rf "$(BUNDLE_DIR)"
	mkdir -p "$(BUNDLE_DIR)/Contents/MacOS" "$(BUNDLE_RES)"
	cp target/release/raflow "$(BUNDLE_BIN)"
	cp packaging/Info.plist "$(BUNDLE_PLIST)"
	/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $(VERSION)" "$(BUNDLE_PLIST)"
	/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $(VERSION)" "$(BUNDLE_PLIST)"
	cp "$(APP_ICON)" "$(BUNDLE_RES)/icon.icns"
	cp THIRD-PARTY-LICENSES.md "$(BUNDLE_RES)/THIRD-PARTY-LICENSES.md"
	codesign --force --deep --sign "$(DEV_CERT_NAME)" "$(BUNDLE_DIR)"
	@echo ""
	@echo "Bundle ready: $(BUNDLE_DIR)"

run-bundle: bundle
	open "$(BUNDLE_DIR)"

# 啟動已安裝的 /Applications/raflow.app（不重 build）。**永遠用 `open` 不要直接 exec
# 二進位** — macOS TCC 的「responsibility chain」會讓直接 exec 的 raflow 借走父層
# terminal（iTerm2）的 Accessibility 授權，掩蓋 raflow 自己沒授權的 bug，於是
# 開發測試「都 ok」但使用者從 Finder 啟動時 enigo 靜默失敗（輸入框沒文字）。
# `open` 走 launchd，TCC 的 responsible 就是 raflow 本身 → 真實反映權限狀態。
run:
	open /Applications/raflow.app

# 把 bundle 複製到 /Applications（需要 admin 權限的檔案夾預設可寫）
install-app: bundle
	rm -rf /Applications/raflow.app
	cp -R "$(BUNDLE_DIR)" /Applications/raflow.app
	@echo "Installed to /Applications/raflow.app"

# Phase 5：含 Whisper 終校的版本。第一次編需要 cmake（brew install cmake）
# 並會編譯 whisper.cpp 約 30~60 秒；之後增量編譯快。詳見 docs/spec/whisper.md
bundle-whisper: icons dev-cert
	cargo build --release -p raflow-app --features whisper
	rm -rf "$(BUNDLE_DIR)"
	mkdir -p "$(BUNDLE_DIR)/Contents/MacOS" "$(BUNDLE_RES)"
	cp target/release/raflow "$(BUNDLE_BIN)"
	cp packaging/Info.plist "$(BUNDLE_PLIST)"
	/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $(VERSION)" "$(BUNDLE_PLIST)"
	/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $(VERSION)" "$(BUNDLE_PLIST)"
	cp "$(APP_ICON)" "$(BUNDLE_RES)/icon.icns"
	cp THIRD-PARTY-LICENSES.md "$(BUNDLE_RES)/THIRD-PARTY-LICENSES.md"
	codesign --force --deep --sign "$(DEV_CERT_NAME)" "$(BUNDLE_DIR)"
	@echo ""
	@echo "Bundle ready (with Whisper): $(BUNDLE_DIR)"

install-app-whisper: bundle-whisper
	rm -rf /Applications/raflow.app
	cp -R "$(BUNDLE_DIR)" /Applications/raflow.app
	@echo "Installed (with Whisper) to /Applications/raflow.app"
	@echo "→ 模型偏好序：$(WHISPER_MODEL_TURBO) > $(WHISPER_MODEL_SMALL)（存在即自動選用；都缺則回退 Apple-only）"

# 下載 Whisper small model（466 MB）+ CoreML encoder（M1 加速必需）到預設路徑
whisper-model-small:
	mkdir -p "$(WHISPER_MODEL_DIR)"
	@if [ ! -f "$(WHISPER_MODEL_SMALL)" ]; then \
		echo "Downloading ggml-small.bin (~466 MB) → $(WHISPER_MODEL_SMALL)"; \
		curl -L --fail --output "$(WHISPER_MODEL_SMALL)" "$(WHISPER_MODEL_SMALL_URL)"; \
	else \
		echo "ggml-small.bin already at $(WHISPER_MODEL_SMALL)"; \
	fi
	@if [ ! -d "$(WHISPER_COREML_SMALL)" ]; then \
		echo "Downloading ggml-small-encoder.mlmodelc.zip (CoreML M1 acceleration)"; \
		curl -L --fail --output "$(WHISPER_MODEL_DIR)/ggml-small-encoder.mlmodelc.zip" "$(WHISPER_COREML_SMALL_URL)"; \
		echo "Unzipping to $(WHISPER_COREML_SMALL)"; \
		unzip -o "$(WHISPER_MODEL_DIR)/ggml-small-encoder.mlmodelc.zip" -d "$(WHISPER_MODEL_DIR)"; \
		rm "$(WHISPER_MODEL_DIR)/ggml-small-encoder.mlmodelc.zip"; \
	else \
		echo "CoreML encoder already at $(WHISPER_COREML_SMALL)"; \
	fi
	@echo ""
	@echo "Whisper model ready. 啟用：make install-app-whisper"

# 下載 Whisper large-v3-turbo q5_0（~547 MB，Metal GPU）到預設路徑。
# raflow 啟動時模型偏好序：turbo > small（檔案存在即自動選用，無需設定）。
whisper-model-turbo:
	mkdir -p "$(WHISPER_MODEL_DIR)"
	@if [ ! -f "$(WHISPER_MODEL_TURBO)" ]; then \
		echo "Downloading ggml-large-v3-turbo-q5_0.bin (~547 MB) → $(WHISPER_MODEL_TURBO)"; \
		curl -L --fail --output "$(WHISPER_MODEL_TURBO)" "$(WHISPER_MODEL_TURBO_URL)"; \
	else \
		echo "ggml-large-v3-turbo-q5_0.bin already at $(WHISPER_MODEL_TURBO)"; \
	fi
	@echo ""
	@echo "Turbo model ready. 啟用：make install-app-whisper（啟動自動偏好 turbo）"

# 下載 Silero VAD 模型（~1 MB）到預設路徑。Phase 1 抗幻覺串流用。
whisper-vad-model:
	mkdir -p "$(WHISPER_MODEL_DIR)"
	@if [ ! -f "$(WHISPER_VAD_MODEL)" ]; then \
		echo "Downloading ggml-silero-v6.2.0.bin (~1 MB) → $(WHISPER_VAD_MODEL)"; \
		curl -L --fail --output "$(WHISPER_VAD_MODEL)" "$(WHISPER_VAD_MODEL_URL)"; \
	else \
		echo "VAD model already at $(WHISPER_VAD_MODEL)"; \
	fi
	@echo ""
	@echo "VAD model ready. 跑隔離 harness：make vad-harness WAV=/path/to/16k.wav"

# Phase 1 抗幻覺隔離 harness：靜音/雜訊零幻覺 + 可選語音準確度關卡。
# 需先 make whisper-model-small 與 make whisper-vad-model。
# 用法：make vad-harness              （只跑靜音/雜訊關卡）
#       make vad-harness WAV=x.wav   （加跑語音關卡）
vad-harness:
	cargo run -p raflow-speech --example vad_harness --features whisper -- $(WAV)

# Phase 2 滾動離線驗證素材：macOS say（Meijia，zh-TW）合成 16kHz mono WAV。
# 素材可隨時重生，不入 repo（testdata/ 在 .gitignore）。
TTS_DIR := testdata/tts
tts-fixtures:
	mkdir -p "$(TTS_DIR)"
	@gen() { say -v Meijia -o "$(TTS_DIR)/_$$1.aiff" "$$2" && \
		afconvert -f WAVE -d LEI16@16000 -c 1 "$(TTS_DIR)/_$$1.aiff" "$(TTS_DIR)/$$1.wav" && \
		rm "$(TTS_DIR)/_$$1.aiff"; }; \
	gen t1 "我們公司的 CI CD 工具是 ArgoCD"; \
	gen t2 "然後我們會使用 Terraform 來管理基礎設施"; \
	gen t3 "第一句話講完了"; \
	gen t4 "接下來是第二句話"; \
	gen t5 "這是一段連續說話中間完全不停頓的比較長的句子"; \
	gen t6 "今天天氣很好 逗點 換行 明天會更好"
	@echo "TTS fixtures ready in $(TTS_DIR)/"

# Phase 2 滾動離線模擬：逐 tick 重放 rolling_tick_core，自動驗證 + 延遲量測。
# 需先 make whisper-model-turbo whisper-vad-model tts-fixtures。
rolling-harness:
	cargo run -p raflow-speech --example rolling_harness --features whisper --release

clean:
	cargo clean
	rm -rf "$(ICONSET)" "$(APP_ICON)"
	rm -f $(ICONS_DIR)/menubar-*.png
