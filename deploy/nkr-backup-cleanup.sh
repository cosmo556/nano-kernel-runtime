#!/bin/bash
# =============================================================================
# nkr-backup-cleanup — Borra TODO en /mnt/nkr/backups/
# =============================================================================
# Llamado por cron a la 1 AM. Retention = 1 día.
# El operador es responsable de transferir off-site antes de esta hora.
#
# Safety:
#   - Skip si hay un nkr-backup en curso (mtime <60s en algún dir)
#   - Lock para no correr 2 cleanups concurrentes
# =============================================================================
set -e

BACKUP_ROOT=/mnt/nkr/backups
LOCK=/run/nkr/backup-cleanup.lock

mkdir -p /run/nkr
exec 200>"$LOCK"
if ! flock -n 200; then
    echo "[NKR-CLEANUP] otro cleanup en curso, saliendo" >&2
    exit 0
fi

if [ ! -d "$BACKUP_ROOT" ]; then
    echo "[NKR-CLEANUP] $BACKUP_ROOT no existe — nada que limpiar"
    exit 0
fi

# Skip si hay un backup en curso (algún dir modificado en último minuto)
RECENT=$(find "$BACKUP_ROOT" -type f -mmin -1 2>/dev/null | head -1)
if [ -n "$RECENT" ]; then
    echo "[NKR-CLEANUP] WARN: archivo modificado <1 min ($RECENT) — backup en curso, abortando" >&2
    exit 1
fi

# Stats antes
BEFORE_SIZE=$(du -sh "$BACKUP_ROOT" 2>/dev/null | cut -f1)
BEFORE_COUNT=$(find "$BACKUP_ROOT" -type d -mindepth 2 -maxdepth 2 2>/dev/null | wc -l)

echo "[NKR-CLEANUP] $(date -Iseconds) — borrando $BEFORE_COUNT backups ($BEFORE_SIZE) en $BACKUP_ROOT"

# Borrar TODO — los dirs de tenant + sus subdirs timestamped
find "$BACKUP_ROOT" -mindepth 1 -maxdepth 1 -type d | while read tenant_dir; do
    rm -rf "$tenant_dir"
done

echo "[NKR-CLEANUP] $(date -Iseconds) — done. Backup root vacío."
