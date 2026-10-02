#!/usr/bin/env bash
# Instala o opencode-compact-if-big em ~/.local/bin (ou $XDG_BIN_HOME).
# Idempotente. Uso: ./install.sh
#
# Esta é a implementação em Rust (zero dependências). O script compila o
# binário e instala o artefato de `target/release/`.
set -euo pipefail

AQUI="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
BIN="$AQUI/target/release/opencode-compact-if-big"

if [ ! -x "$BIN" ]; then
  echo "Compilando (cargo build --release)..."
  cargo build --release --manifest-path "$AQUI/Cargo.toml"
fi

if [ ! -x "$BIN" ]; then
  echo "erro: $BIN não encontrado após a compilação." >&2
  exit 1
fi

mkdir -p "$DEST_DIR"
install -m 755 "$BIN" "$DEST_DIR/opencode-compact-if-big"

echo "Instalado: $DEST_DIR/opencode-compact-if-big"
echo
echo "  relatório (dry-run): opencode-compact-if-big --list"
echo "  TUI:                 opencode-compact-if-big --tui"
echo "  aplicar:             opencode-compact-if-big --above 600k --apply"
echo
echo "Requisitos: sqlite3 e o binário opencode. Nenhuma dependência externa."
