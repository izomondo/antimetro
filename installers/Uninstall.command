#!/usr/bin/env bash
set -euo pipefail

echo "==================================================="
echo "  Uninstalling Antimetro from Antigravity 2.0"
echo "==================================================="
echo ""

PLUGIN_DIR="${HOME}/.gemini/config/plugins/antimetro"
BIN_DST="${PLUGIN_DIR}/sidecars/antimetro/antimetro"

pkill -f "${BIN_DST}" 2>/dev/null || true
sleep 0.3

if [ -d "${PLUGIN_DIR}" ]; then
  rm -rf "${PLUGIN_DIR}"
  echo "[OK] Antimetro plugin files removed."
else
  echo "[INFO] Antimetro is not currently installed."
fi

echo ""
echo "Note: Your notes and per-chat settings in"
echo "  ~/.gemini/antigravity/sidecar_data/antimetro"
echo "were kept safe. To remove all data completely, delete that folder manually."
echo "Fully restart Antigravity to finish."
echo ""
if [ -t 0 ]; then
  read -r -p "Press Enter to close..."
fi
