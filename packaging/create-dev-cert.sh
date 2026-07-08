#!/usr/bin/env bash
#
# 建立 self-signed Code Signing 憑證並匯入 login keychain。
#
# 為什麼需要這個：
#   `codesign --sign -`（ad-hoc）每次 rebuild 產生不同的 cdhash，TCC 把每次 rebuild
#   都當成「不同的 app」要求重新授權（麥克風 / Speech / Accessibility / Input
#   Monitoring）。改用同一顆 self-signed cert 後，TCC 可依 cert 的 designated
#   requirement 認 app，rebuild 不會 reset 授權。
#
# 此腳本 idempotent：cert 已存在就跳過。執行不需要 sudo（只動 login keychain）。
# 詳見 docs/spec/packaging.md（待 spec 化）/ Makefile §dev-cert。

set -euo pipefail

CERT_NAME="${1:-raflow-dev}"

LOGIN_KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
if [[ ! -f "$LOGIN_KEYCHAIN" ]]; then
    LOGIN_KEYCHAIN="$HOME/Library/Keychains/login.keychain"
fi

# 設定私鑰的 partition list，讓 Apple 工具（codesign）能**非互動**存取私鑰。
# 為什麼需要：`security import -A` 只設 legacy app-ACL；現代 macOS（Sierra+）另有
# partition list 這道閘，未設時 CLI codesign 每次都彈 SecurityAgent 授權窗，背景/CI
# 會 hang（曾害背景發佈苦等 45 分鐘、零產出）。設定後對此 keychain 內金鑰永久免彈窗。
# 需 keychain 密碼：取自 $RAFLOW_KEYCHAIN_PW 或互動 TTY 提示；兩者皆無則**跳過並警告**
# （不讓 cert 建立因此失敗；release.sh 的 ensure_codesign_ready 會再守一次）。
set_partition_list() {
    local pw="${RAFLOW_KEYCHAIN_PW:-}"
    if [[ -z "$pw" && -t 0 ]]; then
        read -rsp "  設定 codesign 免彈窗需登入 keychain 密碼（Enter 跳過）: " pw; echo
    fi
    if [[ -z "$pw" ]]; then
        echo "  $(printf '\033[33mℹ\033[0m') 未提供 keychain 密碼，跳過 partition-list 設定"
        echo "    （首次 codesign 會彈窗；一次性免彈窗修法見 scripts/release.sh 的 ensure_codesign_ready）"
        return 0
    fi
    security unlock-keychain -p "$pw" "$LOGIN_KEYCHAIN" >/dev/null 2>&1 || true
    if security set-key-partition-list -S apple-tool:,apple: -s -k "$pw" "$LOGIN_KEYCHAIN" >/dev/null 2>&1; then
        echo "  → partition-list 已設，codesign 可非互動存取私鑰"
    else
        echo "  $(printf '\033[33mℹ\033[0m') partition-list 設定未成功（密碼可能有誤），首次 codesign 仍可能彈窗"
    fi
    pw=""
}

# 把 cert 標為 user trust domain 的 codeSigning trusted root，使 macOS TCC 把
# raflow-dev 簽章視為「合法簽章」而非等同 ad-hoc → 授權才能跨啟動持久化。
# 此函式 idempotent；security 工具自身會跳過已存在的 trust setting。
trust_cert_for_codesigning() {
    local cert_pem="$1"
    if security add-trusted-cert -p codeSign -k "$LOGIN_KEYCHAIN" "$cert_pem" 2>&1 \
        | tee /tmp/raflow-trust-cert.log \
        | grep -q "already exist"; then
        echo "  (trust setting 已存在，跳過)"
    else
        echo "  → 已加入 user trust domain（codeSigning purpose）"
    fi
}

if security find-certificate -c "$CERT_NAME" >/dev/null 2>&1; then
    echo "✓ '$CERT_NAME' 已存在於 keychain"
    # 即使 cert 已存在，也補強 trust 設定（之前版本沒做這步）
    EXISTING_PEM="$(mktemp -t raflow-cert-XXXXXX).pem"
    trap 'rm -f "$EXISTING_PEM"' EXIT
    security find-certificate -c "$CERT_NAME" -p > "$EXISTING_PEM"
    echo "→ 確認 trust 設定..."
    trust_cert_for_codesigning "$EXISTING_PEM"
    echo "→ 確認 codesign 免彈窗（partition-list）..."
    set_partition_list
    exit 0
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

# Code Signing 需要明確的 extendedKeyUsage = codeSigning + digitalSignature
cat > "$TMP_DIR/openssl.cnf" <<EOF
[req]
distinguished_name = req_dn
prompt = no
x509_extensions = code_sign
[req_dn]
CN = $CERT_NAME
[code_sign]
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
keyUsage = critical, digitalSignature
EOF

echo "→ 產生 RSA 2048 self-signed cert（10 年有效）..."
openssl req -x509 -newkey rsa:2048 \
    -keyout "$TMP_DIR/key.pem" \
    -out "$TMP_DIR/cert.pem" \
    -days 3650 -nodes \
    -config "$TMP_DIR/openssl.cnf" \
    >/dev/null 2>&1

echo "→ 打包成 PKCS#12..."
# OpenSSL 3.x 預設用 PBES2 + AES，macOS `security` 不認；`-legacy` 強制用舊 PBE/RC2
# 以相容 Apple keychain。SHA-1 MAC 在私人 dev cert 用途下安全性無問題。
# 用非空 transient 密碼：`security import` 對空密碼處理不穩，確保 MAC 對齊。
TRANSIENT_PASS="raflow-dev-transient"
openssl pkcs12 -export -legacy \
    -inkey "$TMP_DIR/key.pem" \
    -in "$TMP_DIR/cert.pem" \
    -out "$TMP_DIR/bundle.p12" \
    -password "pass:$TRANSIENT_PASS" \
    -name "$CERT_NAME" \
    >/dev/null 2>&1

# -A 讓任何 app 都能用此私鑰簽章（包含 codesign）；不加會在每次 build 時 prompt
echo "→ 匯入 login keychain..."
security import "$TMP_DIR/bundle.p12" \
    -k "$LOGIN_KEYCHAIN" \
    -P "$TRANSIENT_PASS" -A \
    >/dev/null

echo "→ 加入 user trust domain（讓 macOS 把此 cert 簽出來的 app 視為合法簽章）..."
trust_cert_for_codesigning "$TMP_DIR/cert.pem"

echo "→ 設定 partition-list（讓 codesign 非互動免彈窗）..."
set_partition_list

echo ""
echo "✓ 已建立 '$CERT_NAME' 並匯入 login keychain"
echo "  之後 \`make bundle\` / \`make bundle-whisper\` 都會用此 cert 簽章。"
echo "  下一次 rebuild + install 時會再 prompt 一次權限（首次認新 cert 的 designated"
echo "  requirement），授權後 TCC 會記住，後續 rebuild 不會 reset。"
