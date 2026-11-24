#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PLUGIN_DIR="${HOME}/.gemini/config/plugins/antimetro"
SIDECAR_DIR="${PLUGIN_DIR}/sidecars/antimetro"
ASSETS_DIR="${PLUGIN_DIR}/assets"
BIN_DST="${SIDECAR_DIR}/antimetro"

echo "1/3 Building Antimetro v1.2.0 (Rust release)..."
cd "${ROOT_DIR}"
cargo build --release

echo "2/3 Installing plugin into ${PLUGIN_DIR}..."
mkdir -p "${SIDECAR_DIR}" "${ASSETS_DIR}"
pkill -f "${BIN_DST}" 2>/dev/null || true

cp "${ROOT_DIR}/target/release/antimetro" "${BIN_DST}"
chmod +x "${BIN_DST}"
cp "${ROOT_DIR}/assets/logo.svg" "${ASSETS_DIR}/logo.svg"

BIN_JSON="${BIN_DST//\\/\\\\}"
BIN_JSON="${BIN_JSON//\"/\\\"}"

cat > "${PLUGIN_DIR}/plugin.json" <<'EOF'
{
  "name": "antimetro",
  "displayName": "Antimetro",
  "version": "1.2.0",
  "description": "Live context token meter, smart auto-compact, per-chat notes, and full transcript search for Antigravity App 2.0.",
  "logo": "assets/logo.svg",
  "author": {
    "name": "izomondo",
    "url": "https://izomondo.com"
  },
  "homepage": "https://izomondo.com",
  "repository": "https://github.com/izomondo/antimetro"
}
EOF

cat > "${SIDECAR_DIR}/sidecar.json" <<EOF
{
  "command": "${BIN_JSON}",
  "args": [],
  "env": {},
  "restart_policy": "always",
  "display_name": "Antimetro",
  "description": "Live context token meter, smart auto-compact, per-chat notes, and full transcript search.",
  "has_web_ui": true,
  "agent_permissions": {
    "allows_user_interactions": false
  },
  "ui_config": {
    "display_name": "Antimetro",
    "views": [
      {
        "path": "/",
        "entrypoint": "SIDECAR_UI_ENTRYPOINT_AUX_PANE",
        "title": "Antimetro"
      }
    ]
  }
}
EOF

echo "3/3 DONE! Antimetro v1.2.0 installed."
echo "Fully restart Antigravity to load the plugin."
