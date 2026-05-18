#!/bin/bash
# =============================================================================
# nkr-restore-nkr — Restaurar un backup_nkr en cualquier cell de la misma versión
# =============================================================================
# Uso: nkr-restore-nkr <backup_dir> [--as NEW_NAME] [--to-cell CELL]
#
# Flujo:
#   1. Validar backup_info.json + SHA256SUMS
#   2. Validar compat (odoo_version match con target cell)
#   3. Determinar target_cell + new_nkr_name (defaults = origen)
#   4. Si el target tenant existe → ERROR (usar --force para sobrescribir)
#   5. Crear tenant fresh vía la API NKR (regenera SSO secret, DB_HOST, etc.)
#   6. Stop el nuevo tenant
#   7. Drop DB vacía + pg_restore del db.dump
#   8. Untar filestore + addons + pylibs en el dir nuevo
#   9. Renombrar paths internos si el nombre cambió (filestore/<old> → filestore/<new>)
#  10. cell up → verify HTTP responde
#
# Cell-portable: si --to-cell es OTRA cell de la misma versión Odoo, el restore
# sucede ahí. Cell-specific settings (DB_HOST, SSO secret) los regenera NKR.
# =============================================================================
set -e
set -o pipefail

# ─── Args ─────────────────────────────────────────────────────────────────
BACKUP_DIR=""
NEW_NAME=""
TARGET_CELL=""
FORCE=0
TOKEN_FILE="/etc/nkr/api.env"

while [ $# -gt 0 ]; do
    case "$1" in
        --as)        NEW_NAME="$2"; shift 2 ;;
        --to-cell)   TARGET_CELL="$2"; shift 2 ;;
        --force)     FORCE=1; shift ;;
        --token-file) TOKEN_FILE="$2"; shift 2 ;;
        -h|--help)
            cat <<EOF
Usage: nkr-restore-nkr <backup_dir> [options]

Restaura un backup_nkr en cualquier cell de la misma versión Odoo.

Args:
  backup_dir        Path al dir del backup (ej: /mnt/nkr/backups/<name>/<ts>/nkr/)

Options:
  --as NAME         Restorear con otro nkr_name (default: el original)
  --to-cell CELL    Restorear en otra cell (default: la cell origen)
  --force           Sobrescribir si el target existe (DELETE primero)
  --token-file F    Path al api.env con NKR_API_TOKEN (default: /etc/nkr/api.env)

Examples:
  # Restore in-place
  nkr-restore-nkr /mnt/nkr/backups/odoo-v19-foo/2026-05-18-0300/nkr/

  # Clone con nuevo nombre
  nkr-restore-nkr <backup>/nkr/ --as foo-clone

  # Migrar a otra cell de la misma versión
  nkr-restore-nkr <backup>/nkr/ --as foo --to-cell odoo-v19-second
EOF
            exit 0
            ;;
        -*) echo "ERROR: flag desconocida: $1" >&2; exit 1 ;;
        *)  BACKUP_DIR="$1"; shift ;;
    esac
done

if [ -z "$BACKUP_DIR" ]; then
    echo "ERROR: falta backup_dir" >&2
    exit 1
fi
BACKUP_DIR=$(realpath "$BACKUP_DIR")
if [ ! -d "$BACKUP_DIR" ]; then
    echo "ERROR: $BACKUP_DIR no existe o no es dir" >&2
    exit 2
fi
if [ ! -f "$BACKUP_DIR/backup-info.json" ]; then
    echo "ERROR: $BACKUP_DIR/backup-info.json no existe — ¿es realmente un backup_nkr dir?" >&2
    exit 2
fi

# ─── 1. Validar SHA256SUMS ────────────────────────────────────────────────
echo "[NKR-RESTORE] Validating SHA256SUMS..."
cd "$BACKUP_DIR"
if [ -f SHA256SUMS ]; then
    if ! sha256sum -c SHA256SUMS --quiet 2>&1; then
        echo "ERROR: checksums NO matchean — backup corrupto" >&2
        exit 3
    fi
    echo "[NKR-RESTORE]   ✅ Checksums OK"
else
    echo "[NKR-RESTORE]   ⚠️  Sin SHA256SUMS (backup antiguo? continuando sin verify)"
fi

# ─── 2. Parsear backup-info.json ──────────────────────────────────────────
ORIG_NAME=$(python3 -c "import json; print(json.load(open('backup-info.json'))['nkr_name'])")
ORIG_CELL=$(python3 -c "import json; print(json.load(open('backup-info.json'))['cell_origin'])")
ORIG_DB=$(python3 -c "import json; print(json.load(open('backup-info.json'))['db_name'])")
ODOO_VERSION=$(python3 -c "import json; print(json.load(open('backup-info.json'))['odoo_version'])")

# Defaults: same name/cell as origin
[ -z "$NEW_NAME" ]  && NEW_NAME="$ORIG_NAME"
[ -z "$TARGET_CELL" ] && TARGET_CELL="$ORIG_CELL"

echo "[NKR-RESTORE] Restore plan:"
echo "  origen: $ORIG_NAME (cell=$ORIG_CELL, odoo=$ODOO_VERSION)"
echo "  target: $NEW_NAME (cell=$TARGET_CELL)"

# ─── 3. Validar compat odoo_version ───────────────────────────────────────
if [ ! -f "/mnt/nkr/cells/${TARGET_CELL}/cell.yml" ]; then
    echo "ERROR: target cell '$TARGET_CELL' no existe" >&2
    exit 4
fi
TARGET_VER=$(grep "^odoo_version:" "/mnt/nkr/cells/${TARGET_CELL}/cell.yml" | awk -F"'" '{print $2}')
if [ "$TARGET_VER" != "$ODOO_VERSION" ]; then
    echo "ERROR: version mismatch — backup es $ODOO_VERSION, target cell es $TARGET_VER" >&2
    exit 5
fi
echo "[NKR-RESTORE]   ✅ odoo_version match ($ODOO_VERSION)"

# ─── 4. Token API ─────────────────────────────────────────────────────────
if [ ! -f "$TOKEN_FILE" ]; then
    echo "ERROR: token file $TOKEN_FILE no existe" >&2
    exit 6
fi
TOKEN=$(awk -F= '/NKR_API_TOKEN/ {print $2}' "$TOKEN_FILE")
if [ -z "$TOKEN" ]; then
    echo "ERROR: no pude extraer NKR_API_TOKEN de $TOKEN_FILE" >&2
    exit 6
fi
API="http://127.0.0.1:9090/api/v1"

# ─── 5. Check si el target existe ─────────────────────────────────────────
TARGET_FULL="${TARGET_CELL}-${NEW_NAME#${TARGET_CELL}-}"  # asegurar prefijo correcto
EXISTS=$(curl -sS -o /dev/null -w "%{http_code}" \
    -H "Authorization: Bearer $TOKEN" \
    "$API/cells/$TARGET_CELL/instances/$TARGET_FULL")
if [ "$EXISTS" = "200" ]; then
    if [ $FORCE -eq 0 ]; then
        echo "ERROR: target '$TARGET_FULL' ya existe en $TARGET_CELL — usar --force para sobrescribir" >&2
        exit 7
    fi
    echo "[NKR-RESTORE] Target existe + --force → DELETE primero..."
    curl -sS -X DELETE -H "Authorization: Bearer $TOKEN" \
        "$API/cells/$TARGET_CELL/instances/$TARGET_FULL?drop_db=true" > /dev/null
    # Wait for delete to finish (async)
    echo "[NKR-RESTORE]   Esperando que delete termine..."
    for i in $(seq 1 24); do
        sleep 5
        CHECK=$(curl -sS -o /dev/null -w "%{http_code}" \
            -H "Authorization: Bearer $TOKEN" \
            "$API/cells/$TARGET_CELL/instances/$TARGET_FULL")
        if [ "$CHECK" = "404" ]; then
            echo "[NKR-RESTORE]   ✅ Delete completo"
            break
        fi
    done
fi

# ─── 6. Leer admin_passwd del backup (para el create) ─────────────────────
ADMIN_PASSWD=$(awk -F= '/^[[:space:]]*admin_passwd[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2}' \
    "$BACKUP_DIR/odoo.conf" | head -1)
if [ -z "$ADMIN_PASSWD" ] || [ "$ADMIN_PASSWD" = "admin" ]; then
    # Si era default o no parseó, generar uno seguro
    ADMIN_PASSWD=$(openssl rand -hex 16)
    echo "[NKR-RESTORE]   admin_passwd no parseado, generando nuevo: $ADMIN_PASSWD"
fi

# Determinar tier desde meta.json o odoo.conf
TIER="dev"
if [ -f "$BACKUP_DIR/meta.json" ]; then
    META_TIER=$(python3 -c "import json; m=json.load(open('$BACKUP_DIR/meta.json')); print(m.get('tier', ''))" 2>/dev/null)
    [ -n "$META_TIER" ] && TIER="$META_TIER"
fi

# Edition: detectar si tenía web_enterprise (revisar el dump? complicado).
# Simple: leer del meta.json si está.
EDITION="community"
if [ -f "$BACKUP_DIR/meta.json" ]; then
    META_EDITION=$(python3 -c "import json; m=json.load(open('$BACKUP_DIR/meta.json')); print(m.get('edition', ''))" 2>/dev/null)
    [ -n "$META_EDITION" ] && EDITION="$META_EDITION"
fi
ENTERPRISE_FLAG="false"
[ "$EDITION" = "enterprise" ] && ENTERPRISE_FLAG="true"

# ─── 7. Crear nuevo tenant vía API (regenera cell-specific settings) ──────
echo "[NKR-RESTORE] Creando tenant fresh '$TARGET_FULL' (tier=$TIER, enterprise=$ENTERPRISE_FLAG)..."
NEW_SHORT="${NEW_NAME#${TARGET_CELL}-}"
CREATE_RESP=$(curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    "$API/cells/$TARGET_CELL/instances" \
    -d "{
      \"nkr_name\":\"$NEW_SHORT\",
      \"odoo_version\":\"$ODOO_VERSION\",
      \"tier\":\"$TIER\",
      \"enterprise\":$ENTERPRISE_FLAG,
      \"admin_passwd\":\"$ADMIN_PASSWD\"
    }")
CREATE_STATUS=$(echo "$CREATE_RESP" | python3 -c "import json,sys; print(json.load(sys.stdin).get('status', ''))" 2>/dev/null)
if [ "$CREATE_STATUS" != "accepted" ]; then
    echo "ERROR: create falló: $CREATE_RESP" >&2
    exit 8
fi
echo "[NKR-RESTORE]   Create dispatched, esperando ready..."
for i in $(seq 1 30); do
    sleep 5
    STATUS=$(curl -sS -H "Authorization: Bearer $TOKEN" \
        "$API/cells/$TARGET_CELL/instances/$TARGET_FULL/create-status")
    state=$(echo "$STATUS" | python3 -c "import json,sys; print(json.load(sys.stdin).get('status', ''))" 2>/dev/null)
    if [ "$state" = "ready" ]; then break; fi
    if [ "$state" = "failed" ]; then
        echo "ERROR: create falló — $STATUS" >&2
        exit 9
    fi
done

# ─── 8. Stop el nuevo tenant (para drop+restore DB safely) ────────────────
echo "[NKR-RESTORE] Stop $TARGET_FULL para restore DB..."
curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    "$API/cells/$TARGET_CELL/instances/$TARGET_FULL/actions" \
    -d '{"action":"stop"}' > /dev/null
sleep 8

# ─── 9. Resolver target dirs ──────────────────────────────────────────────
TARGET_INST="/mnt/nkr/cells/${TARGET_CELL}/instances/${TARGET_FULL}"
TARGET_CELL_ID=$(grep "^cell_id:" "/mnt/nkr/cells/${TARGET_CELL}/cell.yml" | awk '{print $2}')
TARGET_PG_IP="10.0.${TARGET_CELL_ID}.2"
TARGET_DB="db-${TARGET_FULL}"
PG_PASS=$(awk -F= '/^[[:space:]]*db_password[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2}' \
    "${TARGET_INST}/config/odoo.conf" | head -1)

# ─── 10. Drop + pg_restore ────────────────────────────────────────────────
echo "[NKR-RESTORE] DB: drop empty $TARGET_DB + pg_restore from db.dump..."
T0=$(date +%s)
PGPASSWORD="$PG_PASS" psql -h "$TARGET_PG_IP" -p 5432 -U odoo -d postgres -c \
    "DROP DATABASE IF EXISTS \"$TARGET_DB\";" 2>&1 | head -3
PGPASSWORD="$PG_PASS" psql -h "$TARGET_PG_IP" -p 5432 -U odoo -d postgres -c \
    "CREATE DATABASE \"$TARGET_DB\" OWNER odoo;" 2>&1 | head -3
PGPASSWORD="$PG_PASS" pg_restore -h "$TARGET_PG_IP" -p 5432 -U odoo -d "$TARGET_DB" \
    --no-owner --no-privileges --no-acl "$BACKUP_DIR/db.dump" 2>&1 | head -5 || true
T1=$(date +%s)
echo "[NKR-RESTORE]   ✅ pg_restore en $((T1-T0))s"

# Si el nombre cambió, renombrar filestore_path dentro de la DB
if [ "$ORIG_DB" != "$TARGET_DB" ]; then
    echo "[NKR-RESTORE]   db_name cambió ($ORIG_DB → $TARGET_DB) — nada que hacer en DB (paths son relativos)"
fi

# ─── 11. Untar filestore + addons + pylibs ────────────────────────────────
NEW_SHORT_NAME="${TARGET_FULL#${TARGET_CELL}-}"
NEW_FS_EXT4="/mnt/nkr/cells/${TARGET_CELL}/.nkr-data/${NEW_SHORT_NAME}-var_lib_odoo.ext4"

if [ -f "$BACKUP_DIR/filestore.tar.zst" ] && [ -f "$NEW_FS_EXT4" ]; then
    echo "[NKR-RESTORE] Restaurando filestore..."
    MNT_FS=$(mktemp -d /tmp/nkr-restore-fs-XXX)
    mount -o loop "$NEW_FS_EXT4" "$MNT_FS"
    # Limpiar el filestore vacío del create fresco antes de untar
    rm -rf "$MNT_FS/filestore" 2>/dev/null
    tar -I 'zstd -d' -xf "$BACKUP_DIR/filestore.tar.zst" -C "$MNT_FS"
    # Si el db_name cambió, renombrar el dir filestore/<old> → filestore/<new>
    if [ "$ORIG_DB" != "$TARGET_DB" ] && [ -d "$MNT_FS/filestore/$ORIG_DB" ]; then
        mv "$MNT_FS/filestore/$ORIG_DB" "$MNT_FS/filestore/$TARGET_DB"
        echo "[NKR-RESTORE]   filestore renombrado $ORIG_DB → $TARGET_DB"
    fi
    chown -R 101:101 "$MNT_FS" 2>/dev/null || true
    umount "$MNT_FS" && rmdir "$MNT_FS"
fi

if [ -f "$BACKUP_DIR/addons.tar.zst" ]; then
    echo "[NKR-RESTORE] Restaurando addons..."
    rm -rf "$TARGET_INST/addons"
    tar -I 'zstd -d' -xf "$BACKUP_DIR/addons.tar.zst" -C "$TARGET_INST"
fi

if [ -f "$BACKUP_DIR/pylibs.tar.zst" ]; then
    echo "[NKR-RESTORE] Restaurando pylibs..."
    rm -rf "$TARGET_INST/pylibs/lib"
    mkdir -p "$TARGET_INST/pylibs"
    tar -I 'zstd -d' -xf "$BACKUP_DIR/pylibs.tar.zst" -C "$TARGET_INST/pylibs"
fi

# ─── 12. Restart tenant ───────────────────────────────────────────────────
echo "[NKR-RESTORE] Start $TARGET_FULL..."
curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
    "$API/cells/$TARGET_CELL/instances/$TARGET_FULL/actions" \
    -d '{"action":"start"}' > /dev/null
sleep 20

# ─── 13. Verify ───────────────────────────────────────────────────────────
echo "[NKR-RESTORE] Verificando..."
for i in $(seq 1 12); do
    sleep 5
    INFO=$(curl -sS -H "Authorization: Bearer $TOKEN" \
        "$API/cells/$TARGET_CELL/instances/$TARGET_FULL")
    port=$(echo "$INFO" | python3 -c "import json,sys; print(json.load(sys.stdin).get('nkr_status', {}).get('port_8069_up', False))" 2>/dev/null)
    if [ "$port" = "True" ]; then
        IP=$(echo "$INFO" | python3 -c "import json,sys; print(json.load(sys.stdin)['guest_ip'])")
        echo "[NKR-RESTORE]   ✅ $TARGET_FULL listo (IP=$IP, port_8069 up)"
        echo
        echo "═══════════════════════════════════════════════"
        echo "✅ Restore completo: $TARGET_FULL en $TARGET_CELL"
        echo "═══════════════════════════════════════════════"
        echo "  Origen:    $ORIG_NAME (cell=$ORIG_CELL)"
        echo "  Restored:  $TARGET_FULL (cell=$TARGET_CELL, ip=$IP)"
        echo "  DB:        $TARGET_DB"
        echo "  Login URL: http://$IP:8069/web/login"
        exit 0
    fi
done

echo "WARN: tenant restorado pero port_8069 no responde tras 60s. Verificar logs."
exit 10
