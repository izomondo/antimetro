#!/usr/bin/env bash
set -euo pipefail

echo "==================================================="
echo "  Installing Antimetro v1.2.0 for Antigravity 2.0"
echo "==================================================="
echo ""

SRC_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="${HOME}/.gemini/config/plugins/antimetro"
SIDECAR_DIR="${PLUGIN_DIR}/sidecars/antimetro"
ASSETS_DIR="${PLUGIN_DIR}/assets"
BIN_DST="${SIDECAR_DIR}/antimetro"

if [ ! -f "${SRC_DIR}/antimetro" ]; then
  echo "[ERROR] 'antimetro' binary not found next to this script."
  read -r -p "Press Enter to exit..."
  exit 1
fi

mkdir -p "${SIDECAR_DIR}" "${ASSETS_DIR}"
pkill -f "${BIN_DST}" 2>/dev/null || true

cp "${SRC_DIR}/antimetro" "${BIN_DST}"
chmod +x "${BIN_DST}"
xattr -d com.apple.quarantine "${BIN_DST}" 2>/dev/null || true
cp "${SRC_DIR}/logo.svg" "${ASSETS_DIR}/logo.svg"

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

echo "[OK] Antimetro v1.2.0 has been installed!"
echo ""
echo "Please fully restart Antigravity and open Antimetro in the right Side Panel."
echo ""
if [ -t 0 ]; then
  read -r -p "Press Enter to close..."
fi
