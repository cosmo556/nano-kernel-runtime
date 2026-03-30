#!/bin/bash
# =============================================================================
# NKR Odoo 17 — Setup inicial (ejecutar UNA vez)
# =============================================================================
# Crea los discos ext4 para Odoo 17 + PostgreSQL 15, configura initramfs,
# y prepara el directorio de módulos customizados.
#
# Uso: sudo ./deploy/setup.sh
# =============================================================================

set -e

# ── Configuración ──
NKR_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"
DEPLOY_DIR="$NKR_DIR/deploy"
DISK_DIR="/opt/nkr/disks"
MODULES_DIR="/opt/nkr/modules"
INITRAMFS_DIR="/opt/nkr/initramfs"
BACKUP_DIR="/opt/nkr/backups"
CONFIG_DIR="/opt/nkr/config"
DATA_DIR="/opt/nkr/data"

PG_DISK="$DISK_DIR/postgres.ext4"
ODOO_DISK="$DISK_DIR/odoo-prod.ext4"
PG_INITRAMFS="$INITRAMFS_DIR/pg_initramfs.cpio.gz"
ODOO_INITRAMFS="$INITRAMFS_DIR/odoo_initramfs.cpio.gz"

# Colores
RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'

info()  { echo -e "${GREEN}[SETUP]${NC} $1"; }
warn()  { echo -e "${YELLOW}[SETUP]${NC} $1"; }
error() { echo -e "${RED}[SETUP]${NC} $1"; exit 1; }

# ── Verificar requisitos ──
[[ $EUID -ne 0 ]] && error "Ejecutar con sudo: sudo ./deploy/setup.sh"
[[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado. Ejecuta: cargo build --release"
which docker &>/dev/null || error "Docker es requerido para descargar imágenes OCI"

info "╔══════════════════════════════════════════════════════════════╗"
info "║  NKR Odoo 17 — Setup Inicial                               ║"
info "╚══════════════════════════════════════════════════════════════╝"

# ── Crear directorios ──
mkdir -p "$DISK_DIR" "$MODULES_DIR" "$INITRAMFS_DIR" "$BACKUP_DIR" "$CONFIG_DIR" "$DATA_DIR/pg" "$DATA_DIR/filestore"
info "Directorios creados en /opt/nkr/"

# ── 1. Disco PostgreSQL 15 ──
if [[ -f "$PG_DISK" ]]; then
    warn "Disco PostgreSQL ya existe: $PG_DISK (omitiendo)"
else
    info "Descargando PostgreSQL 15 desde Docker Hub..."
    "$NKR_BIN" pull postgres:15 "$PG_DISK" --size-gb 2
    info "Disco PostgreSQL creado: $PG_DISK"
fi

# ── 2. Disco Odoo 17 ──
if [[ -f "$ODOO_DISK" ]]; then
    warn "Disco Odoo ya existe: $ODOO_DISK (omitiendo)"
else
    info "Descargando Odoo 17 desde Docker Hub..."
    "$NKR_BIN" pull odoo:17.0 "$ODOO_DISK" --size-gb 4
    info "Disco Odoo 17 creado: $ODOO_DISK"
fi

# ── 3. Configuraciones editables ──
# Copiar configs de ejemplo si no existen aún en /opt/nkr/config/
if [[ -d "$DEPLOY_DIR/config" ]]; then
    for cfg_file in "$DEPLOY_DIR/config"/*; do
        [[ ! -f "$cfg_file" ]] && continue
        dest_file="$CONFIG_DIR/$(basename "$cfg_file")"
        if [[ ! -f "$dest_file" ]]; then
            cp "$cfg_file" "$dest_file"
            info "Config copiada: $(basename "$cfg_file") → $CONFIG_DIR/"
        else
            warn "Config ya existe: $dest_file (no se sobreescribe)"
        fi
    done
else
    warn "No se encontró $DEPLOY_DIR/config/. Crea tus configs en $CONFIG_DIR/"
fi
info "Tus configs editables están en $CONFIG_DIR/"
info "  Edítalas y se inyectan vía --volume al arrancar (ver start.sh)"

# ── 4. Initramfs PostgreSQL ──
if [[ -d /tmp/pg_initramfs ]] && [[ -f /tmp/pg_initramfs/init ]]; then
    info "Reconstruyendo PG initramfs desde /tmp/pg_initramfs/..."

    # Limpiar módulos innecesarios para reducir tamaño (solo los esenciales)
    PG_INITRAMFS_SRC="/tmp/pg_initramfs"

    (cd "$PG_INITRAMFS_SRC" && find . | cpio -o -H newc 2>/dev/null | gzip -9 > "$PG_INITRAMFS")
    info "PG initramfs: $PG_INITRAMFS ($(du -h "$PG_INITRAMFS" | cut -f1))"
else
    warn "No se encontró /tmp/pg_initramfs/. Copia manualmente el initramfs a $PG_INITRAMFS"
fi

# ── 5. Initramfs Odoo ──
if [[ -f /tmp/odoo_initramfs.cpio.gz ]]; then
    cp /tmp/odoo_initramfs.cpio.gz "$ODOO_INITRAMFS"
    info "Odoo initramfs copiado: $ODOO_INITRAMFS"
elif [[ -d /tmp/_inspect_odoo ]] && [[ -f /tmp/_inspect_odoo/init ]]; then
    info "Reconstruyendo Odoo initramfs..."
    (cd /tmp/_inspect_odoo && find . | cpio -o -H newc 2>/dev/null | gzip -9 > "$ODOO_INITRAMFS")
    info "Odoo initramfs: $ODOO_INITRAMFS"
else
    warn "No se encontró initramfs de Odoo. Copia manualmente a $ODOO_INITRAMFS"
fi

# ── 6. Crear módulo de ejemplo ──
if [[ ! -f "$MODULES_DIR/__placeholder__" ]]; then
    cat > "$MODULES_DIR/__placeholder__" << 'EOF'
# Este directorio contiene módulos Odoo customizados.
# Tu webhook de GitHub debe sincronizar aquí.
#
# Estructura esperada:
# /opt/nkr/modules/
# ├── mi_modulo/
# │   ├── __manifest__.py
# │   ├── __init__.py
# │   └── models/
# └── otro_modulo/
#     └── ...
#
# Después de actualizar, ejecuta:
#   sudo ./deploy/update.sh
EOF
    info "Directorio de módulos listo: $MODULES_DIR/"
fi

# ── 7. Permisos ──
chmod +x "$DEPLOY_DIR"/*.sh 2>/dev/null || true

# ── Resumen ──
echo ""
info "╔══════════════════════════════════════════════════════════════╗"
info "║  Setup completado                                          ║"
info "╠══════════════════════════════════════════════════════════════╣"
info "║  Discos:                                                   ║"
info "║    PostgreSQL: $PG_DISK"
info "║    Odoo 17:    $ODOO_DISK"
info "║                                                            ║"
info "║  Initramfs:                                                ║"
info "║    PG:   $PG_INITRAMFS"
info "║    Odoo: $ODOO_INITRAMFS"
info "║                                                            ║"
info "║  Módulos custom: $MODULES_DIR/"
info "║                                                            ║"
info "║  Siguiente paso:                                           ║"
info "║    sudo ./deploy/start.sh                                  ║"
info "╚══════════════════════════════════════════════════════════════╝"
