#!/bin/bash
# =============================================================================
# NKR Odoo 17 — Actualización rápida de módulos (~5 segundos de downtime)
# =============================================================================
#
# Sincroniza módulos desde /opt/nkr/modules/ al disco Odoo y reinicia.
# PostgreSQL NO se toca — solo Odoo se reinicia.
#
# Uso:
#   sudo ./deploy/update.sh              # Actualizar producción (~5s downtime)
#   sudo ./deploy/update.sh --test       # Probar con copia en puerto 8070
#   sudo ./deploy/update.sh --rollback   # Restaurar backup anterior
#   sudo ./deploy/update.sh --update-db  # Actualizar + forzar -u all en Odoo
#
# Tu webhook de GitHub solo necesita:
#   1. git pull en /opt/nkr/modules/
#   2. sudo ./deploy/update.sh
# =============================================================================

set -e

NKR_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"
BZIMAGE="$NKR_DIR/bzImage"
DISK_DIR="/opt/nkr/disks"
INITRAMFS_DIR="/opt/nkr/initramfs"
MODULES_DIR="/opt/nkr/modules"
BACKUP_DIR="/opt/nkr/backups"

ODOO_DISK="$DISK_DIR/odoo-prod.ext4"
ODOO_TEST_DISK="$DISK_DIR/odoo-test.ext4"
ODOO_INITRAMFS="$INITRAMFS_DIR/odoo_initramfs.cpio.gz"

ODOO_ID=2         # Producción: IP 10.0.0.3
TEST_ODOO_ID=10   # Test: IP 10.0.0.11

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${GREEN}[UPDATE]${NC} $1"; }
warn()  { echo -e "${YELLOW}[UPDATE]${NC} $1"; }
error() { echo -e "${RED}[UPDATE]${NC} $1"; exit 1; }
timer() { echo -e "${CYAN}[UPDATE]${NC} $1"; }

[[ $EUID -ne 0 ]] && error "Ejecutar con sudo"
[[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado"

# ── Función: sincronizar módulos a un disco ──
sync_modules_to_disk() {
    local DISK="$1"
    local MOUNT_POINT=$(mktemp -d /tmp/nkr_update.XXXXXX)

    mount -o loop "$DISK" "$MOUNT_POINT"

    # Crear directorio de módulos si no existe
    mkdir -p "$MOUNT_POINT/mnt/extra-addons"

    # Rsync: sincronizar solo directorios con __manifest__.py (módulos Odoo válidos)
    local MODULE_COUNT=0
    for mod_dir in "$MODULES_DIR"/*/; do
        if [[ -f "$mod_dir/__manifest__.py" ]] || [[ -f "$mod_dir/__openerp__.py" ]]; then
            rsync -a --delete "$mod_dir" "$MOUNT_POINT/mnt/extra-addons/$(basename "$mod_dir")/"
            MODULE_COUNT=$((MODULE_COUNT + 1))
        fi
    done

    # Permisos para usuario odoo (UID 101 en imagen Docker)
    chown -R 101:101 "$MOUNT_POINT/mnt/extra-addons" 2>/dev/null || true

    umount "$MOUNT_POINT"
    rmdir "$MOUNT_POINT"

    echo "$MODULE_COUNT"
}

# ── Función: hacer backup de módulos actuales ──
backup_current_modules() {
    local DISK="$1"
    local TIMESTAMP=$(date +%Y%m%d_%H%M%S)
    local BACKUP_FILE="$BACKUP_DIR/modules_$TIMESTAMP.tar.gz"
    local MOUNT_POINT=$(mktemp -d /tmp/nkr_backup.XXXXXX)

    mount -o loop,ro "$DISK" "$MOUNT_POINT"

    if [[ -d "$MOUNT_POINT/mnt/extra-addons" ]] && [[ "$(ls -A "$MOUNT_POINT/mnt/extra-addons" 2>/dev/null)" ]]; then
        tar -czf "$BACKUP_FILE" -C "$MOUNT_POINT/mnt/extra-addons" . 2>/dev/null
        info "Backup creado: $BACKUP_FILE"
    fi

    umount "$MOUNT_POINT"
    rmdir "$MOUNT_POINT"

    # Mantener solo los últimos 5 backups
    ls -t "$BACKUP_DIR"/modules_*.tar.gz 2>/dev/null | tail -n +6 | xargs rm -f 2>/dev/null
}

# ── Función: restaurar desde backup ──
restore_from_backup() {
    local DISK="$1"
    local LATEST_BACKUP=$(ls -t "$BACKUP_DIR"/modules_*.tar.gz 2>/dev/null | head -1)

    if [[ -z "$LATEST_BACKUP" ]]; then
        error "No hay backups disponibles en $BACKUP_DIR/"
    fi

    info "Restaurando desde: $LATEST_BACKUP"
    local MOUNT_POINT=$(mktemp -d /tmp/nkr_restore.XXXXXX)

    mount -o loop "$DISK" "$MOUNT_POINT"
    rm -rf "$MOUNT_POINT/mnt/extra-addons"/*
    tar -xzf "$LATEST_BACKUP" -C "$MOUNT_POINT/mnt/extra-addons/"
    chown -R 101:101 "$MOUNT_POINT/mnt/extra-addons" 2>/dev/null || true
    umount "$MOUNT_POINT"
    rmdir "$MOUNT_POINT"

    info "Módulos restaurados."
}

# =============================================================================
# Modo: --test — Probar módulos en puerto 8070 sin tocar producción
# =============================================================================
if [[ "$1" == "--test" ]]; then
    info "╔══════════════════════════════════════════════════════════════╗"
    info "║  Modo TEST — Probando módulos en puerto 8070               ║"
    info "╚══════════════════════════════════════════════════════════════╝"

    # Matar test anterior si existe
    "$NKR_BIN" stop "$TEST_ODOO_ID" 2>/dev/null || true

    # Copiar disco de producción como base para test
    info "Copiando disco de producción para test..."
    cp "$ODOO_DISK" "$ODOO_TEST_DISK"

    # Sincronizar módulos nuevos al disco de test
    info "Sincronizando módulos..."
    COUNT=$(sync_modules_to_disk "$ODOO_TEST_DISK")
    info "$COUNT módulo(s) sincronizado(s) al disco de test"

    # Configurar odoo.conf de test para conectar al PG de prod (10.0.0.2)
    MOUNT_POINT=$(mktemp -d /tmp/nkr_testconf.XXXXXX)
    mount -o loop "$ODOO_TEST_DISK" "$MOUNT_POINT"
    # db_host sigue siendo 10.0.0.2 (PG de prod) — compartir DB para test
    sed -i 's/^db_host.*/db_host = 10.0.0.2/' "$MOUNT_POINT/etc/odoo/odoo.conf"
    # Usar una DB diferente para test
    sed -i 's/^db_name.*/db_name = odoo_test/' "$MOUNT_POINT/etc/odoo/odoo.conf"
    sed -i 's/^dbfilter.*/dbfilter = odoo_test/' "$MOUNT_POINT/etc/odoo/odoo.conf"
    umount "$MOUNT_POINT"
    rmdir "$MOUNT_POINT"

    # Lanzar VM de test
    info "Lanzando Odoo de test en puerto 8070..."
    "$NKR_BIN" run \
        --id "$TEST_ODOO_ID" \
        --ram 512 \
        --chrs 1 \
        --disk "$ODOO_TEST_DISK" \
        --kernel "$BZIMAGE" \
        --initramfs "$ODOO_INITRAMFS" \
        --port "8070:8069" \
        &

    info "╔══════════════════════════════════════════════════════════════╗"
    info "║  Test lanzado: http://<tu-ip>:8070                         ║"
    info "║                                                            ║"
    info "║  Cuando termines:                                          ║"
    info "║    sudo ./deploy/stop.sh --test     (detener test)         ║"
    info "║    sudo ./deploy/update.sh          (aplicar a producción) ║"
    info "╚══════════════════════════════════════════════════════════════╝"
    exit 0
fi

# =============================================================================
# Modo: --rollback — Restaurar módulos anteriores
# =============================================================================
if [[ "$1" == "--rollback" ]]; then
    info "╔══════════════════════════════════════════════════════════════╗"
    info "║  Modo ROLLBACK — Restaurando versión anterior              ║"
    info "╚══════════════════════════════════════════════════════════════╝"

    START_TIME=$(date +%s%N)

    # Detener Odoo
    info "Deteniendo Odoo..."
    "$NKR_BIN" stop "$ODOO_ID" 2>/dev/null || true
    sleep 1

    # Restaurar
    restore_from_backup "$ODOO_DISK"

    # Reiniciar Odoo
    info "Reiniciando Odoo..."
    "$NKR_BIN" run \
        --id "$ODOO_ID" \
        --ram 1024 \
        --chrs 2 \
        --disk "$ODOO_DISK" \
        --kernel "$BZIMAGE" \
        --initramfs "$ODOO_INITRAMFS" \
        --port "8069:8069" \
        &

    END_TIME=$(date +%s%N)
    ELAPSED=$(( (END_TIME - START_TIME) / 1000000 ))

    timer "Rollback completado en ${ELAPSED}ms"
    exit 0
fi

# =============================================================================
# Modo: default — Actualizar producción
# =============================================================================
info "╔══════════════════════════════════════════════════════════════╗"
info "║  Actualizando módulos en PRODUCCIÓN                        ║"
info "╚══════════════════════════════════════════════════════════════╝"

# Verificar que hay módulos para sincronizar
MODS_FOUND=$(find "$MODULES_DIR" -maxdepth 2 -name "__manifest__.py" 2>/dev/null | wc -l)
if [[ $MODS_FOUND -eq 0 ]]; then
    warn "No se encontraron módulos Odoo en $MODULES_DIR/"
    warn "Cada módulo debe tener __manifest__.py"
    warn "Continuando de todas formas (limpiará addons existentes si no hay módulos)..."
fi

START_TIME=$(date +%s%N)

# 1. Backup de módulos actuales
info "Respaldando módulos actuales..."
backup_current_modules "$ODOO_DISK"

# 2. Detener Odoo (PG sigue corriendo)
info "Deteniendo Odoo..."
"$NKR_BIN" stop "$ODOO_ID" 2>/dev/null || true
sleep 1

# 3. Sincronizar módulos
info "Sincronizando módulos..."
COUNT=$(sync_modules_to_disk "$ODOO_DISK")

# 4. Reiniciar Odoo
info "Reiniciando Odoo..."

ODOO_CMD="$NKR_BIN run \
    --id $ODOO_ID \
    --ram 1024 \
    --chrs 2 \
    --disk $ODOO_DISK \
    --kernel $BZIMAGE \
    --initramfs $ODOO_INITRAMFS \
    --port 8069:8069"

$ODOO_CMD &

END_TIME=$(date +%s%N)
ELAPSED=$(( (END_TIME - START_TIME) / 1000000 ))

# Matar instancia de test si estaba corriendo
"$NKR_BIN" stop "$TEST_ODOO_ID" 2>/dev/null || true

timer "╔══════════════════════════════════════════════════════════════╗"
timer "║  Actualización completada en ${ELAPSED}ms                  "
timer "║  $COUNT módulo(s) sincronizado(s)                          "
timer "║  Odoo reiniciando en http://<tu-ip>:8069                   "
timer "╚══════════════════════════════════════════════════════════════╝"
