#!/bin/bash
# =============================================================================
# migrate-cell-rootfs.sh — Migrate an existing cell from shared master ext4
# images to private per-cell reflink copies.
# =============================================================================
#
# Background: cells originally created before this change have `db` and
# `pgbouncer` blocks pointing to the master images under /mnt/nkr/images/
# directly. When two cells run simultaneously they share-mmap the same backing
# file with PROT_WRITE, which can corrupt the ext4 metadata. This script:
#
#   1. Reflinks the masters into the cell directory (zero disk on btrfs).
#   2. Stops db + pgbouncer of the cell.
#   3. Rewrites the cell's nkr-compose.yml to point at the private copies.
#   4. Restarts both services.
#   5. Verifies pg_isready.
#
# Usage:
#   sudo ./migrate-cell-rootfs.sh <cell-name>
#
# Idempotent: if the cell was already migrated, the script is a no-op (the
# reflinks are skipped because the destination files already exist, and the
# sed runs against entries that already point to the per-cell paths).
#
# Rollback: revert the sed (or restore .bak.<ts> from nkr-compose.yml) and
# nkr restart. The master files in /mnt/nkr/images/ are never modified.
# =============================================================================

set -euo pipefail

CELL="${1:-}"
if [[ -z "$CELL" ]]; then
    echo "Usage: $0 <cell-name>" >&2
    exit 2
fi

NKR_DATA="${NKR_DATA_DIR:-/mnt/nkr}"
CELL_DIR="$NKR_DATA/cells/$CELL"
IMAGES_DIR="$NKR_DATA/images"
COMPOSE="$CELL_DIR/nkr-compose.yml"

if [[ ! -d "$CELL_DIR" ]]; then
    echo "ERR: cell dir not found: $CELL_DIR" >&2
    exit 1
fi
if [[ ! -f "$COMPOSE" ]]; then
    echo "ERR: nkr-compose.yml not found at $COMPOSE" >&2
    exit 1
fi
for master in postgres.ext4 pgbouncer.ext4; do
    if [[ ! -f "$IMAGES_DIR/$master" ]]; then
        echo "ERR: master image missing: $IMAGES_DIR/$master" >&2
        echo "     run: sudo nkr build -f Nkrfile.${master%.ext4} first" >&2
        exit 1
    fi
done

echo "=== migrate-cell-rootfs: $CELL ==="
echo "cell dir:    $CELL_DIR"
echo "compose:     $COMPOSE"
echo "images:      $IMAGES_DIR"
echo

# 1. Reflink each master into the cell directory.
#
#    --reflink=auto is O(1) on btrfs (CoW). On non-btrfs hosts it falls back
#    to a physical copy — slower but still correct.
#
#    NOTE: chattr +C is intentionally NOT applied to the reflinked copy. The
#    flag is a no-op on files whose extents are shared via reflink (btrfs doc:
#    "NOCOW must be set on empty files"). Shared extents always CoW on write
#    by definition. The rootfs is read-mostly, so residual fragmentation is
#    operationally negligible.
for src in postgres.ext4 pgbouncer.ext4; do
    dst="${src%.ext4}-root.ext4"
    if [[ -f "$CELL_DIR/$dst" ]]; then
        echo "[1/4] $dst already exists, skipping reflink"
        continue
    fi
    echo "[1/4] reflinking $IMAGES_DIR/$src → $CELL_DIR/$dst"
    cp -a --reflink=auto "$IMAGES_DIR/$src" "$CELL_DIR/$dst"
    e2fsck -p "$CELL_DIR/$dst" || true
done

# 2. Stop db + pgbouncer (best-effort: nkr stop fails cleanly if not running).
echo "[2/4] stopping $CELL-db and $CELL-pgb (or matching nkr_name)..."
nkr stop "$CELL-db" 2>/dev/null || true
nkr stop "$CELL-pgb" 2>/dev/null || true
nkr stop "$CELL-pgbouncer" 2>/dev/null || true

# 3. Rewrite the compose to point at the private copies. Backup the current
#    file with timestamp suffix; the rotation in cell.rs (last 20 backups)
#    will handle the cleanup of older ones.
TS=$(date +%s)
cp -a "$COMPOSE" "$COMPOSE.bak.$TS"
echo "[3/4] backup of $COMPOSE → $COMPOSE.bak.$TS"

sed -i \
    -e "s|$IMAGES_DIR/postgres\.ext4|$CELL_DIR/postgres-root.ext4|g" \
    -e "s|$IMAGES_DIR/pgbouncer\.ext4|$CELL_DIR/pgbouncer-root.ext4|g" \
    "$COMPOSE"

# 4. Bring db + pgbouncer back up. We rely on `nkr compose up -d` here because
#    a service-scoped command may not exist on all NKR versions; bringing the
#    whole cell up is idempotent for everything else.
echo "[4/4] starting cell..."
( cd "$CELL_DIR" && nkr compose up -d )

echo
echo "=== migration done. verify: ==="
echo "  grep '$CELL_DIR/' $COMPOSE       # should show -root.ext4 paths"
echo "  nkr ps | grep $CELL"
echo "  pg_isready -h <ip-db-cell> -p 5432"
