#!/usr/bin/env bash
# Instala o opencode-compact-if-big em ~/.local/bin (ou $XDG_BIN_HOME).
# Idempotente. Uso: ./install.sh
set -euo pipefail

AQUI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
mkdir -p "$DEST_DIR"
install -m 755 "$AQUI/opencode-compact-if-big" "$DEST_DIR/opencode-compact-if-big"

echo "Instalado: $DEST_DIR/opencode-compact-if-big"
echo
echo "  relatório (dry-run): opencode-compact-if-big --list"
echo "  TUI:                 opencode-compact-if-big --tui"
echo "  aplicar:             opencode-compact-if-big --above 600k --apply"
echo
echo "Requisitos: python3 (3.8+) e o binário opencode. Nenhuma dependência externa."
