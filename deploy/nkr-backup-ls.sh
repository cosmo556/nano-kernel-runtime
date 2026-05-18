#!/bin/bash
# =============================================================================
# nkr-backup-ls — Lista backups disponibles
# =============================================================================
# Uso: nkr-backup-ls [tenant_name]
#
# Sin args: lista todos los backups de todos los tenants
# Con tenant_name: lista solo los de ese tenant
# =============================================================================
set -e

BACKUP_ROOT=/mnt/nkr/backups

if [ ! -d "$BACKUP_ROOT" ] || [ -z "$(ls -A "$BACKUP_ROOT" 2>/dev/null)" ]; then
    echo "Sin backups en $BACKUP_ROOT"
    exit 0
fi

FILTER=""
[ $# -ge 1 ] && FILTER="$1"

printf "%-40s %-18s %-10s %-10s %-6s\n" "TENANT" "TIMESTAMP" "NKR_SIZE" "ODOO_SIZE" "AGE"
echo "─────────────────────────────────────────────────────────────────────────────────────────"

for tenant_dir in "$BACKUP_ROOT"/*/; do
    tenant=$(basename "$tenant_dir")
    [ -n "$FILTER" ] && [ "$tenant" != "$FILTER" ] && continue

    for ts_dir in "$tenant_dir"*/; do
        [ ! -d "$ts_dir" ] && continue
        ts=$(basename "$ts_dir")

        nkr_size="—"
        odoo_size="—"
        [ -d "${ts_dir}nkr" ] && nkr_size=$(du -sh "${ts_dir}nkr" 2>/dev/null | cut -f1)
        [ -d "${ts_dir}odoo" ] && odoo_size=$(du -sh "${ts_dir}odoo" 2>/dev/null | cut -f1)

        # Age desde mtime del dir
        age_min=$(( ($(date +%s) - $(stat -c %Y "$ts_dir")) / 60 ))
        if [ $age_min -lt 60 ]; then
            age="${age_min}m"
        elif [ $age_min -lt 1440 ]; then
            age="$((age_min / 60))h"
        else
            age="$((age_min / 1440))d"
        fi

        printf "%-40s %-18s %-10s %-10s %-6s\n" "$tenant" "$ts" "$nkr_size" "$odoo_size" "$age"
    done
done

echo
echo "Total: $(du -sh "$BACKUP_ROOT" 2>/dev/null | cut -f1) — cleanup automático: 1 AM"
