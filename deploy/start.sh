#!/bin/bash
# =============================================================================
# NKR Odoo 17 — Iniciar stack (PostgreSQL + Odoo)
# =============================================================================
# Inicia PG y Odoo como VMs independientes usando nkr run.
# PG arranca primero y se espera a que esté listo antes de lanzar Odoo.
#
# Uso: sudo ./deploy/start.sh
# =============================================================================

set -e

NKR_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"
BZIMAGE="$NKR_DIR/bzImage"
DISK_DIR="/opt/nkr/disks"
INITRAMFS_DIR="/opt/nkr/initramfs"
CONFIG_DIR="/opt/nkr/config"
DATA_DIR="/opt/nkr/data"

PG_DISK="$DISK_DIR/postgres.ext4"
ODOO_DISK="$DISK_DIR/odoo-prod.ext4"
PG_INITRAMFS="$INITRAMFS_DIR/pg_initramfs.cpio.gz"
ODOO_INITRAMFS="$INITRAMFS_DIR/odoo_initramfs.cpio.gz"
ODOO_CONF="$CONFIG_DIR/odoo.conf"

# Crear directorios de datos si no existen
mkdir -p "$DATA_DIR/pg" "$DATA_DIR/filestore"

# IDs fijos para producción
PG_ID=1      # IP: 10.0.0.2
ODOO_ID=2    # IP: 10.0.0.3

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[START]${NC} $1"; }
warn()  { echo -e "${YELLOW}[START]${NC} $1"; }
error() { echo -e "${RED}[START]${NC} $1"; exit 1; }

[[ $EUID -ne 0 ]] && error "Ejecutar con sudo"
[[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado"
[[ ! -f "$PG_DISK" ]] && error "Disco PG no existe. Ejecuta: sudo ./deploy/setup.sh"
[[ ! -f "$ODOO_DISK" ]] && error "Disco Odoo no existe. Ejecuta: sudo ./deploy/setup.sh"

# Verificar si ya están corriendo
RUNNING=$("$NKR_BIN" ps 2>&1 | grep -c "║" || true)
if [[ $RUNNING -gt 2 ]]; then
    warn "Ya hay VMs corriendo. Usa 'sudo ./deploy/stop.sh' primero."
    "$NKR_BIN" ps
    exit 1
fi

info "╔══════════════════════════════════════════════════════════════╗"
info "║  NKR Odoo 17 — Iniciando Stack                               ║"
info "╚══════════════════════════════════════════════════════════════╝"

# ── 1. Iniciar PostgreSQL ──
info "Iniciando PostgreSQL (id=$PG_ID, IP=10.0.0.2)..."

"$NKR_BIN" run \
    --id "$PG_ID" \
    --ram 512 \
    --chrs 1 \
    --disk "$PG_DISK" \
    --kernel "$BZIMAGE" \
    --initramfs "$PG_INITRAMFS" \
    --port "5432:5432" \
    --volume "$DATA_DIR/pg:/var/lib/postgresql/data:rw" \
    &

PG_PID=$!
info "PostgreSQL lanzado (PID host: $PG_PID)"

# Esperar a que PG esté lista (check TCP 10.0.0.2:5432)
info "Esperando a que PostgreSQL esté lista..."
for i in $(seq 1 30); do
    sleep 2
    if timeout 1 bash -c "echo > /dev/tcp/10.0.0.2/5432" 2>/dev/null; then
        info "PostgreSQL lista (${i}x2s)"
        break
    fi
    if [[ $i -eq 30 ]]; then
        warn "PostgreSQL no respondió en 60s. Continuando de todas formas..."
    fi
done

# ── 2. Iniciar Odoo ──
info "Iniciando Odoo 17 (id=$ODOO_ID, IP=10.0.0.3)..."

"$NKR_BIN" run \
    --id "$ODOO_ID" \
    --ram 1024 \
    --chrs 2 \
    --disk "$ODOO_DISK" \
    --kernel "$BZIMAGE" \
    --initramfs "$ODOO_INITRAMFS" \
    --port "8069:8069" \
    --volume "$ODOO_CONF:/etc/odoo/odoo.conf" \
    --volume "/opt/nkr/modules:/mnt/extra-addons" \
    --volume "$DATA_DIR/filestore:/var/lib/odoo:rw" \
    &

ODOO_PID=$!
info "Odoo lanzado (PID host: $ODOO_PID)"

sleep 2
info "╔══════════════════════════════════════════════════════════════╗"
info "║  Stack iniciado                                              ║"
info "║  PostgreSQL: 10.0.0.2:5432  (host :5432)                     ║"
info "║  Odoo 17:    10.0.0.3:8069  (host :8069)                     ║"
info "║                                                              ║"
info "║  Abrir: http://<tu-ip>:8069                                  ║"
info "║  Actualizar módulos: sudo ./deploy/update.sh                 ║"
info "║  Detener: sudo ./deploy/stop.sh                              ║"
info "╚══════════════════════════════════════════════════════════════╝"

# Esperar a ambos procesos (el script queda aquí)
wait $PG_PID $ODOO_PID 2>/dev/null || true
info "Stack finalizado."
