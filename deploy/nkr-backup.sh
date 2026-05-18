#!/bin/bash
# =============================================================================
# nkr-backup — Backup on-demand de un tenant Odoo
# =============================================================================
# Uso: nkr-backup <nkr_name> [--nkr-only|--odoo-only] [--output DIR]
#
# Genera dos formatos en paralelo (default):
#   1. backup_nkr  — interno NKR (tar.zst + pg_dump -Fc) → restorable en
#                    cualquier cell de la misma versión Odoo via `nkr restore-nkr`
#   2. backup_odoo — formato estándar Odoo (ZIP con dump.sql + filestore +
#                    manifest.json) → restorable en cualquier Odoo via
#                    /web/database/restore (no NKR-specific)
#
# Output: /mnt/nkr/backups/<nkr_name>/<YYYY-MM-DD-HHMM>/{nkr,odoo}/
#
# Retention: NINGUNA. Cron a la 1 AM borra TODO /mnt/nkr/backups/.
# El operador es responsable de transferir off-site antes del cleanup.
#
# Concurrencia: toma flock sobre el InstanceLock del tenant (mismo lock
# que usa delete/restart de NKR) para evitar race conditions.
# =============================================================================
set -e
set -o pipefail

# ─── Args ─────────────────────────────────────────────────────────────────
GEN_NKR=1
GEN_ODOO=1
OUTPUT_BASE=""
NKR_NAME=""

while [ $# -gt 0 ]; do
    case "$1" in
        --nkr-only)  GEN_ODOO=0; shift ;;
        --odoo-only) GEN_NKR=0;  shift ;;
        --output)    OUTPUT_BASE="$2"; shift 2 ;;
        -h|--help)
            cat <<EOF
Usage: nkr-backup <nkr_name> [options]

Options:
  --nkr-only        Solo backup_nkr (interno)
  --odoo-only       Solo backup_odoo (entregable estándar)
  --output DIR      Output dir custom (default: /mnt/nkr/backups/<name>/<ts>/)
  -h, --help        Esta ayuda
EOF
            exit 0
            ;;
        -*) echo "ERROR: flag desconocida: $1" >&2; exit 1 ;;
        *) NKR_NAME="$1"; shift ;;
    esac
done

if [ -z "$NKR_NAME" ]; then
    echo "ERROR: falta nkr_name" >&2
    echo "Usage: nkr-backup <nkr_name>" >&2
    exit 1
fi

# ─── Resolver cell, paths, password ───────────────────────────────────────
CELL=""
INST_DIR=""
for cell_dir in /mnt/nkr/cells/*/; do
    cell=$(basename "$cell_dir")
    if [ -d "${cell_dir}instances/${NKR_NAME}" ]; then
        CELL="$cell"
        INST_DIR="${cell_dir}instances/${NKR_NAME}"
        break
    fi
done

if [ -z "$CELL" ]; then
    echo "ERROR: tenant '$NKR_NAME' no encontrado en ninguna cell" >&2
    exit 2
fi

# Prefijo: si nkr_name empieza con "<cell>-" sacarlo para el short name
SHORT="${NKR_NAME#${CELL}-}"

ODOO_VERSION=$(grep "^odoo_version:" "/mnt/nkr/cells/${CELL}/cell.yml" | awk -F"'" '{print $2}')
CELL_ID=$(grep "^cell_id:" "/mnt/nkr/cells/${CELL}/cell.yml" | awk '{print $2}')
PG_IP="10.0.${CELL_ID}.2"
DB_NAME="db-${NKR_NAME}"

if [ -z "$ODOO_VERSION" ] || [ -z "$CELL_ID" ]; then
    echo "ERROR: no pude leer cell.yml de $CELL" >&2
    exit 3
fi

# Password: del odoo.conf del tenant
if [ ! -f "${INST_DIR}/config/odoo.conf" ]; then
    echo "ERROR: ${INST_DIR}/config/odoo.conf no existe" >&2
    exit 4
fi
PG_PASS=$(awk -F= '/^[[:space:]]*db_password[[:space:]]*=/ {gsub(/[[:space:]]/, "", $2); print $2}' "${INST_DIR}/config/odoo.conf" | head -1)
if [ -z "$PG_PASS" ]; then
    echo "ERROR: no pude extraer db_password de odoo.conf" >&2
    exit 4
fi

# ─── Output dir ───────────────────────────────────────────────────────────
TS=$(date +%Y-%m-%d-%H%M)
if [ -z "$OUTPUT_BASE" ]; then
    OUTPUT_BASE="/mnt/nkr/backups/${NKR_NAME}/${TS}"
fi
mkdir -p "$OUTPUT_BASE"

NKR_DIR="$OUTPUT_BASE/nkr"
ODOO_DIR="$OUTPUT_BASE/odoo"

# ─── Lock — coordinar con delete/restart de NKR ───────────────────────────
LOCK_FILE="/run/nkr/instances/${CELL}_${NKR_NAME}.lock"
mkdir -p /run/nkr/instances
exec 200>"$LOCK_FILE"
echo "[NKR-BACKUP] Adquiriendo lock $LOCK_FILE..."
if ! flock -w 30 200; then
    echo "ERROR: no pude adquirir lock — hay delete/restart/backup en curso" >&2
    exit 5
fi
echo "[NKR-BACKUP] Lock OK"
echo "[NKR-BACKUP] Backup de $NKR_NAME (cell=$CELL, db=$DB_NAME, odoo=$ODOO_VERSION)"

# ─── Mount filestore RO (común a ambos formatos) ──────────────────────────
FILESTORE_EXT4="/mnt/nkr/cells/${CELL}/.nkr-data/${SHORT}-var_lib_odoo.ext4"
MNT_FS=""
if [ -f "$FILESTORE_EXT4" ]; then
    MNT_FS=$(mktemp -d /tmp/nkr-backup-fs-XXX)
    mount -o ro,loop,noload "$FILESTORE_EXT4" "$MNT_FS"
    echo "[NKR-BACKUP] Filestore montado RO en $MNT_FS"
fi

# Cleanup trap — siempre desmonta el filestore aunque algo falle
cleanup() {
    if [ -n "$MNT_FS" ] && mountpoint -q "$MNT_FS" 2>/dev/null; then
        umount "$MNT_FS" 2>/dev/null || umount -l "$MNT_FS"
        rmdir "$MNT_FS" 2>/dev/null
    fi
}
trap cleanup EXIT

# ─── backup_nkr ───────────────────────────────────────────────────────────
if [ $GEN_NKR -eq 1 ]; then
    mkdir -p "$NKR_DIR"
    cd "$NKR_DIR"
    echo "[NKR-BACKUP] === Generando backup_nkr ==="
    START=$(date +%s)

    # 1. backup-info.json
    KERNEL_SHA=$(sha256sum /mnt/nkr/kernel/nanolinux 2>/dev/null | awk '{print $1}')
    NKR_VER=$(/usr/local/bin/nkr --version 2>/dev/null | awk '{print $2}')
    cat > backup-info.json <<EOF
{
  "format": "backup_nkr",
  "schema_version": 1,
  "nkr_name": "$NKR_NAME",
  "short_name": "$SHORT",
  "cell_origin": "$CELL",
  "cell_id": $CELL_ID,
  "db_name": "$DB_NAME",
  "odoo_version": "$ODOO_VERSION",
  "nkr_version": "$NKR_VER",
  "kernel_sha256": "$KERNEL_SHA",
  "backup_ts": "$TS",
  "backup_started_at": $START
}
EOF

    # 2. meta.json + odoo.conf (copias literales para referencia en restore)
    [ -f "${INST_DIR}/meta.json" ] && cp "${INST_DIR}/meta.json" .
    cp "${INST_DIR}/config/odoo.conf" .

    # 3. pg_dump -Fc (custom format, internamente comprimido)
    echo "[NKR-BACKUP]   pg_dump (-Fc)..."
    T0=$(date +%s)
    PGPASSWORD="$PG_PASS" pg_dump -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" \
        -Fc -Z 5 -f db.dump
    T1=$(date +%s)
    DB_SIZE=$(du -h db.dump | cut -f1)
    echo "[NKR-BACKUP]     → $DB_SIZE en $((T1-T0))s"

    # 4. Filestore (si existe el ext4 share)
    if [ -n "$MNT_FS" ]; then
        echo "[NKR-BACKUP]   filestore (tar zstd)..."
        T0=$(date +%s)
        # NOTE: el contenido REAL del filestore vive en filestore/<db_name>/ adentro
        # del var_lib_odoo. Hacemos tar de TODO el var_lib_odoo (incluye sessions,
        # ir_attachment binarios, etc.). Tamaño chico en general.
        tar --warning=no-file-changed -I 'zstd -3' -cf filestore.tar.zst -C "$MNT_FS" . 2>/dev/null || true
        T1=$(date +%s)
        FS_SIZE=$(du -h filestore.tar.zst | cut -f1)
        echo "[NKR-BACKUP]     → $FS_SIZE en $((T1-T0))s"
    else
        echo "[NKR-BACKUP]   filestore: skip (no hay $FILESTORE_EXT4)"
    fi

    # 5. Addons (custom modules del tenant)
    if [ -d "${INST_DIR}/addons" ] && [ -n "$(ls -A "${INST_DIR}/addons" 2>/dev/null)" ]; then
        echo "[NKR-BACKUP]   addons (tar zstd)..."
        T0=$(date +%s)
        tar -I 'zstd -3' -cf addons.tar.zst -C "$INST_DIR" addons/
        T1=$(date +%s)
        AD_SIZE=$(du -h addons.tar.zst | cut -f1)
        echo "[NKR-BACKUP]     → $AD_SIZE en $((T1-T0))s"
    fi

    # 6. Pylibs (Python deps via PUT /pylibs)
    if [ -d "${INST_DIR}/pylibs/lib" ] && [ -n "$(ls -A "${INST_DIR}/pylibs/lib" 2>/dev/null)" ]; then
        echo "[NKR-BACKUP]   pylibs (tar zstd)..."
        T0=$(date +%s)
        tar -I 'zstd -3' -cf pylibs.tar.zst -C "${INST_DIR}/pylibs" lib/
        T1=$(date +%s)
        PY_SIZE=$(du -h pylibs.tar.zst | cut -f1)
        echo "[NKR-BACKUP]     → $PY_SIZE en $((T1-T0))s"
    fi

    # 7. SHA256SUMS
    sha256sum * > SHA256SUMS 2>/dev/null

    END=$(date +%s)
    TOTAL=$(du -sh . | cut -f1)
    echo "[NKR-BACKUP]   backup_nkr: $TOTAL en $((END-START))s ($NKR_DIR)"
fi

# ─── backup_odoo (formato estándar Odoo: ZIP con dump.sql + filestore + manifest) ──
if [ $GEN_ODOO -eq 1 ]; then
    mkdir -p "$ODOO_DIR"
    cd "$ODOO_DIR"
    echo "[NKR-BACKUP] === Generando backup_odoo (ZIP standard) ==="
    START=$(date +%s)

    STAGING=$(mktemp -d /tmp/nkr-backup-odoo-XXX)

    # 1. dump.sql (plain SQL para portabilidad universal)
    echo "[NKR-BACKUP]   pg_dump (--format=plain)..."
    T0=$(date +%s)
    PGPASSWORD="$PG_PASS" pg_dump -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" \
        --format=plain --no-owner --no-privileges -f "$STAGING/dump.sql"
    T1=$(date +%s)
    SQL_SIZE=$(du -h "$STAGING/dump.sql" | cut -f1)
    echo "[NKR-BACKUP]     → dump.sql $SQL_SIZE en $((T1-T0))s"

    # 2. manifest.json (formato Odoo nativo del /web/database/backup)
    # Pull list of modules installed + creation date via PG
    INSTALLED_MODULES=$(PGPASSWORD="$PG_PASS" psql -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" -t -A -c \
        "SELECT name||','||latest_version FROM ir_module_module WHERE state='installed' ORDER BY name;" 2>/dev/null)
    CREATE_DATE=$(PGPASSWORD="$PG_PASS" psql -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" -t -A -c \
        "SELECT create_date FROM ir_module_module WHERE name='base' LIMIT 1;" 2>/dev/null)
    PG_VER=$(PGPASSWORD="$PG_PASS" psql -h "$PG_IP" -p 5432 -U odoo -d "$DB_NAME" -t -A -c \
        "SHOW server_version_num;" 2>/dev/null)

    # Build manifest.json (compatible con el formato que Odoo nativo produce)
    python3 <<PYEOF > "$STAGING/manifest.json"
import json
modules = {}
for line in """$INSTALLED_MODULES""".strip().split("\n"):
    if "," in line:
        name, ver = line.split(",", 1)
        modules[name] = ver
manifest = {
    "odoo_dump": "1",
    "db_name": "$DB_NAME",
    "version": "$ODOO_VERSION",
    "version_info": [int(x) for x in "$ODOO_VERSION".split(".")] + [0, "final", 0, ""],
    "major_version": "$ODOO_VERSION".split(".")[0] + ".0",
    "pg_version": "${PG_VER:0:2}.0",
    "modules": modules,
}
print(json.dumps(manifest, indent=2))
PYEOF
    echo "[NKR-BACKUP]     → manifest.json ($(wc -l < "$STAGING/manifest.json") líneas, $(echo "$INSTALLED_MODULES" | wc -l) módulos)"

    # 3. filestore/<db_name>/ — directorio de attachments
    if [ -n "$MNT_FS" ] && [ -d "$MNT_FS/filestore/$DB_NAME" ]; then
        echo "[NKR-BACKUP]   copiando filestore..."
        T0=$(date +%s)
        mkdir -p "$STAGING/filestore"
        cp -a "$MNT_FS/filestore/$DB_NAME" "$STAGING/filestore/"
        T1=$(date +%s)
        FS_SIZE=$(du -sh "$STAGING/filestore" | cut -f1)
        echo "[NKR-BACKUP]     → filestore $FS_SIZE en $((T1-T0))s"
    elif [ -n "$MNT_FS" ]; then
        # Filestore puede no existir si el tenant nunca subió attachments
        echo "[NKR-BACKUP]   filestore: vacío (no hay $MNT_FS/filestore/$DB_NAME)"
    fi

    # 4. ZIP final (formato Odoo nativo)
    echo "[NKR-BACKUP]   ZIP'ing..."
    T0=$(date +%s)
    ZIP_NAME="${DB_NAME}-${TS}.zip"
    # python3 zipfile preserva mtimes y es portable
    python3 -c "
import zipfile, os, sys
src = '$STAGING'
out = '$ODOO_DIR/$ZIP_NAME'
with zipfile.ZipFile(out, 'w', zipfile.ZIP_DEFLATED, compresslevel=6) as zf:
    for root, dirs, files in os.walk(src):
        for f in files:
            full = os.path.join(root, f)
            arc = os.path.relpath(full, src)
            zf.write(full, arc)
print(f'ZIP OK: {os.path.getsize(out)} bytes')
"
    T1=$(date +%s)
    rm -rf "$STAGING"
    ZIP_SIZE=$(du -h "$ZIP_NAME" | cut -f1)
    echo "[NKR-BACKUP]     → $ZIP_NAME $ZIP_SIZE en $((T1-T0))s"

    END=$(date +%s)
    TOTAL=$(du -sh . | cut -f1)
    echo "[NKR-BACKUP]   backup_odoo: $TOTAL en $((END-START))s ($ODOO_DIR)"
fi

# ─── Summary ──────────────────────────────────────────────────────────────
echo
echo "═══════════════════════════════════════════════"
echo "✅ Backup completado: $OUTPUT_BASE"
echo "═══════════════════════════════════════════════"
ls -lh "$OUTPUT_BASE"/*/ 2>/dev/null
echo
echo "Total: $(du -sh "$OUTPUT_BASE" | cut -f1)"
echo "Cleanup automático: 1 AM (cron)"
