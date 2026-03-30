#!/bin/bash
# =============================================================================
# NKR Odoo 17 — Detener stack
# =============================================================================
# Detiene todas las VMs NKR limpiamente usando nkr stop.
#
# Uso: sudo ./deploy/stop.sh [--odoo-only]
# =============================================================================

set -e

NKR_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"

PG_ID=1
ODOO_ID=2
TEST_ODOO_ID=10

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
info() { echo -e "${GREEN}[STOP]${NC} $1"; }

[[ $EUID -ne 0 ]] && { echo -e "${RED}[STOP]${NC} Ejecutar con sudo"; exit 1; }

if [[ "$1" == "--odoo-only" ]]; then
    info "Deteniendo solo Odoo (PG sigue corriendo)..."
    "$NKR_BIN" stop "$ODOO_ID" 2>/dev/null || true
    info "Odoo detenido."
    exit 0
fi

if [[ "$1" == "--test" ]]; then
    info "Deteniendo VM de test..."
    "$NKR_BIN" stop "$TEST_ODOO_ID" 2>/dev/null || true
    info "Test detenido."
    exit 0
fi

info "Deteniendo stack completo..."

# Detener Odoo primero, luego PG
"$NKR_BIN" stop "$ODOO_ID" 2>/dev/null || true
"$NKR_BIN" stop "$TEST_ODOO_ID" 2>/dev/null || true
"$NKR_BIN" stop "$PG_ID" 2>/dev/null || true

# Fallback: matar cualquier proceso nkr restante
sleep 1
pkill -f "nkr run" 2>/dev/null || true

info "Stack detenido."
