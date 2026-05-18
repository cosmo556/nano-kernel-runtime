#!/bin/bash
# =============================================================================
# nkr-backup-rm — Borra un backup específico (antes del cleanup automático)
# =============================================================================
# Uso: nkr-backup-rm <tenant>/<timestamp>
#      nkr-backup-rm <tenant>          (borra TODOS los backups del tenant)
#
# Ejemplos:
#   nkr-backup-rm odoo-v19-intech-devp/2026-05-18-1430
#   nkr-backup-rm odoo-v19-intech-devp
# =============================================================================
set -e

if [ $# -lt 1 ]; then
    echo "Usage: nkr-backup-rm <tenant>[/<timestamp>]" >&2
    exit 1
fi

TARGET="/mnt/nkr/backups/$1"

if [ ! -d "$TARGET" ]; then
    echo "ERROR: $TARGET no existe" >&2
    exit 2
fi

SIZE=$(du -sh "$TARGET" 2>/dev/null | cut -f1)
echo "[NKR-BACKUP-RM] Borrando $TARGET ($SIZE)..."
rm -rf "$TARGET"

# Si el dir padre del tenant quedó vacío, borrarlo también
parent=$(dirname "$TARGET")
if [ "$parent" != "/mnt/nkr/backups" ] && [ -z "$(ls -A "$parent" 2>/dev/null)" ]; then
    rmdir "$parent"
fi

echo "[NKR-BACKUP-RM] ✅ Done"
