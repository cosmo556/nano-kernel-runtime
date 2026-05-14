#!/bin/bash
# =============================================================================
# NKR Cell Setup — Prepara la estructura de directorios para una Célula Odoo
# =============================================================================
#
# Estructura en el host:
#
#   /mnt/nkr/master/rootfs/       ← RootFS Maestro (Odoo code, Python, shared RO)
#   /mnt/nkr/cells/<CELL>/addons/ ← Extra-addons del cliente (RO por compartido)
#   /mnt/nkr/cells/<CELL>/files/  ← Filestore: imágenes, PDFs, sesiones (RW)
#   /mnt/nkr/cells/<CELL>/config/ ← odoo.conf (inyectado como env/volume)
#   /mnt/nkr/cells/<CELL>/logs/   ← Logs de la celda
#   /mnt/nkr/cells/<CELL>/pg/     ← Datos PostgreSQL del cliente
#
# Uso:
#   sudo ./deploy/cell-setup.sh <nombre-celda> [dominio]
#
# Ejemplo:
#   sudo ./deploy/cell-setup.sh C1 erp.cliente1.com
#   sudo ./deploy/cell-setup.sh C2 erp.cliente2.com
#
# =============================================================================

set -e

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${GREEN}[CELL-SETUP]${NC} $1"; }
warn()  { echo -e "${YELLOW}[CELL-SETUP]${NC} $1"; }
error() { echo -e "${RED}[CELL-SETUP]${NC} $1"; exit 1; }

# ── Verificar argumentos ──
CELL_NAME="${1:?Uso: $0 <nombre-celda> [dominio]}"
DOMAIN="${2:-erp.${CELL_NAME,,}.local}"

[[ $EUID -ne 0 ]] && error "Ejecutar con sudo: sudo $0 $CELL_NAME $DOMAIN"

NKR_DIR="$(cd "$(dirname "$0")/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"
NKR_DATA="${NKR_DATA_DIR:-/mnt/nkr}"

MASTER_ROOTFS="$NKR_DATA/master/rootfs"
CELL_DIR="$NKR_DATA/cells/$CELL_NAME"

info "╔══════════════════════════════════════════════════════════════╗"
info "║  NKR Cell Setup — Célula: $CELL_NAME"
info "║  Dominio: $DOMAIN"
info "╚══════════════════════════════════════════════════════════════╝"

# =============================================================================
# 1. RootFS Maestro (compartido, se crea solo una vez)
# =============================================================================

if [[ -d "$MASTER_ROOTFS" ]] && [[ -f "$MASTER_ROOTFS/usr/bin/python3" ]]; then
    info "RootFS Maestro ya existe: $MASTER_ROOTFS"
else
    info "Creando RootFS Maestro desde imagen Odoo 17..."
    mkdir -p "$MASTER_ROOTFS"

    ODOO_DISK="$NKR_DATA/images/odoo.ext4"
    if [[ ! -f "$ODOO_DISK" ]]; then
        # Descargar imagen si no existe
        [[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado. Ejecuta: cargo build --release"
        which docker &>/dev/null || error "Docker es requerido para descargar imágenes OCI"
        info "Descargando Odoo 17 desde Docker Hub..."
        "$NKR_BIN" pull odoo:17.0 "$ODOO_DISK" --size-gb 4
    fi

    # Extraer rootfs desde el disco ext4
    MOUNT_TMP="/tmp/nkr_rootfs_extract_$$"
    mkdir -p "$MOUNT_TMP"
    mount -o loop,ro "$ODOO_DISK" "$MOUNT_TMP"
    cp -a "$MOUNT_TMP"/. "$MASTER_ROOTFS"/
    umount "$MOUNT_TMP"
    rmdir "$MOUNT_TMP"

    info "RootFS Maestro extraído: $MASTER_ROOTFS"
    info "  Tamaño: $(du -sh "$MASTER_ROOTFS" | cut -f1)"
fi

# =============================================================================
# 2. Registrar Célula en NKR (asigna cell_id + crea cell.yml + bridge)
# =============================================================================

[[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado. Ejecuta: cargo build --release"

# Si la célula ya está registrada, no volver a crearla (idempotente)
if [[ -f "$CELL_DIR/cell.yml" ]]; then
    info "Célula ya registrada: $CELL_DIR/cell.yml"
else
    info "Registrando célula '$CELL_NAME' en NKR..."
    "$NKR_BIN" cell create "$CELL_NAME" --odoo-version 17.0 \
        || error "No se pudo registrar la célula '$CELL_NAME'"
fi

# Leer cell_id del cell.yml (extracción simple con grep+awk)
CELL_ID="$(grep -E '^cell_id:' "$CELL_DIR/cell.yml" | awk '{print $2}' | tr -d '[:space:]')"
[[ -z "$CELL_ID" ]] && error "No se pudo leer cell_id de $CELL_DIR/cell.yml"
info "cell_id asignado: $CELL_ID (subnet 10.0.${CELL_ID}.0/24, bridge nkr-br${CELL_ID})"

# IPs derivadas del cell_id + vm_id (fórmula: 10.0.{cell_id}.{vm_id+1})
# IDs fijos: pg=1, pgbouncer=2, odoo-NN=3..N
PG_IP="10.0.${CELL_ID}.2"        # vm_id=1
PGB_IP="10.0.${CELL_ID}.3"       # vm_id=2
ODOO_IP="10.0.${CELL_ID}.4"      # vm_id=3 (odoo-01)

# Estructura de la Célula (nkr cell create ya creó estos directorios)
mkdir -p "$CELL_DIR/addons"    # Slot 1: Extra-addons (ReadOnly via VirtIO-FS)
mkdir -p "$CELL_DIR/files"     # Slot 2: Filestore (ReadWrite via VirtIO-FS)
mkdir -p "$CELL_DIR/config"    # Configuración Odoo
mkdir -p "$CELL_DIR/logs"      # Logs del stack
mkdir -p "$CELL_DIR/pg"        # Datos PostgreSQL

# Permisos: odoo user (UID 101 en imagen Odoo oficial)
chown -R 101:101 "$CELL_DIR/files" 2>/dev/null || true
chown -R 101:101 "$CELL_DIR/addons" 2>/dev/null || true
# Permisos: postgres user (UID 999 en imagen PG oficial)
chown -R 999:999 "$CELL_DIR/pg" 2>/dev/null || true

# =============================================================================
# 3. Configuración Odoo (apuntando a PgBouncer de la Célula)
# =============================================================================

# La IP del PgBouncer depende del ID asignado. Para el esquema estándar:
#   PG       = 10.0.X.2 (vm_id calculado por NKR registry)
#   PgBouncer= 10.0.X.4
#   Odoo     = 10.0.X.5
# Usamos environment vars para inyectar dinámicamente.

ODOO_CONF="$CELL_DIR/config/odoo.conf"
if [[ ! -f "$ODOO_CONF" ]]; then
    cat > "$ODOO_CONF" << 'ODOOCONF'
[options]
; =============================================================================
; Odoo 17 — Configuración para NKR Cell (Stateless Worker)
; =============================================================================
; Variables dinámicas (inyectadas por NKR_CLEAN vía environment):
;   $DB_HOST, $DB_PORT, $DB_USER, $DB_PASSWORD
;
; Este archivo se monta vía VirtIO-FS o volume al boot.
; =============================================================================

addons_path = /usr/lib/python3/dist-packages/odoo/addons,/mnt/extra-addons
data_dir = /var/lib/odoo

; ── Base de datos (PgBouncer dentro de la Célula) ──
; Se sobreescribe dinámicamente por el entrypoint si DB_HOST está en el env
db_host = False
db_port = 6432
db_user = odoo
db_password = odoo
admin_passwd = admin

; ── Filtros DB ──
db_maxconn = 64
db_name = False
db_template = template1
dbfilter = .*
list_db = True

; ── Logs ──
log_handler = [':INFO']
log_level = info
logfile = None

; ── Performance (Stateless Worker: sin cron, workers=0 para dev, >0 para prod) ──
max_cron_threads = 0
workers = 0
limit_memory_hard = 2684354560
limit_memory_soft = 2147483648
limit_time_cpu = 60
limit_time_real = 120

; ── HTTP ──
xmlrpc = True
xmlrpc_interface =
xmlrpc_port = 8069

csv_internal_sep = ,

; ── Proxy (cuando nginx está delante) ──
proxy_mode = True
ODOOCONF
    info "Configuración Odoo creada: $ODOO_CONF"
else
    warn "Configuración Odoo ya existe: $ODOO_CONF (no se sobreescribe)"
fi

# =============================================================================
# 4. Generar nkr-compose.yml de la Célula
# =============================================================================

COMPOSE="$CELL_DIR/nkr-compose.yml"
cat > "$COMPOSE" << YAML
# =============================================================================
# NKR Compose — Célula: ${CELL_NAME}
# =============================================================================
# Arquitectura Stateless Worker:
#   - PostgreSQL: almacenamiento persistente
#   - PgBouncer: connection pooler (transaction mode)
#   - Odoo: worker stateless con rootfs compartido (VirtIO-FS RO + DAX)
#
# Slot 0 (rootfs):  RootFS Maestro /mnt/nkr/master/rootfs → / (ReadOnly+DAX)
# Slot 1 (addons):  Módulos del cliente → /opt/odoo/extra-addons (ReadOnly)
# Slot 2 (files):   Filestore → /var/lib/odoo (ReadWrite)
#
# Uso:
#   cd ${CELL_DIR} && sudo nkr compose up -d
#   sudo nkr compose down -f ${CELL_DIR}/nkr-compose.yml
# =============================================================================

services:

  # ---------------------------------------------------------------------------
  # PostgreSQL — Almacén de datos de la célula
  # ---------------------------------------------------------------------------
  db:
    id: 1
    disks:
      - /mnt/nkr/images/postgres.ext4
    ram: 512
    chrs: 1
    nkr_name: "${CELL_NAME}-db"
    shares:
      - "${CELL_DIR}/pg:/var/lib/postgresql/data"
    environment:
      POSTGRES_USER: odoo
      POSTGRES_PASSWORD: odoo
      POSTGRES_DB: postgres
    healthcheck:
      port: 5432
      initial_delay: 10
      interval: 5
      retries: 24

  # ---------------------------------------------------------------------------
  # PgBouncer — Connection pooler (transaction mode)
  # ---------------------------------------------------------------------------
  pgbouncer:
    id: 2
    disks:
      - /mnt/nkr/images/pgbouncer.ext4
    ram: 128
    chrs: 1
    burst: false
    nkr_name: "${CELL_NAME}-pgb"
    environment:
      PG_HOST: "${PG_IP}"
      PG_PORT: "5432"
      PG_USER: odoo
      PG_PASSWORD: odoo
      POOL_MODE: transaction
      POOL_SIZE: "20"
      MAX_CLIENT_CONN: "400"
    healthcheck:
      port: 6432
      initial_delay: 15
      interval: 5
      retries: 20

  # ---------------------------------------------------------------------------
  # Odoo — Worker Stateless
  # ---------------------------------------------------------------------------
  odoo:
    id: 3
    rootfs: "${MASTER_ROOTFS}"
    ram: 512
    chrs: 1
    nkr_name: "${CELL_NAME}-odoo-01"
    shares:
      - "${CELL_DIR}/addons:/mnt/extra-addons:ro"
      - "${CELL_DIR}/files:/var/lib/odoo:rw"
    volumes:
      - "${CELL_DIR}/config/odoo.conf:/etc/odoo/odoo.conf"
    environment:
      DB_HOST: "${PGB_IP}"
      DB_PORT: "6432"
      DB_NAME: "False"
      DB_USER: odoo
      DB_PASSWORD: odoo
    healthcheck:
      port: 8069
      initial_delay: 20
      interval: 5
      retries: 30
YAML

info "Compose generado: $COMPOSE"

# =============================================================================
# Resumen
# =============================================================================

echo ""
info "╔══════════════════════════════════════════════════════════════╗"
info "║  Célula ${CELL_NAME} preparada                             "
info "╠══════════════════════════════════════════════════════════════╣"
info "║  Estructura de archivos:"
info "║"
info "║  $NKR_DATA/"
info "║  ├── master/rootfs/          ← Código Odoo compartido (RO+DAX)"
info "║  └── cells/${CELL_NAME}/"
info "║      ├── addons/             ← Módulos del cliente (VirtIO-FS RO)"
info "║      ├── files/              ← Filestore: fotos, PDFs (VirtIO-FS RW)"
info "║      ├── config/odoo.conf    ← Configuración Odoo"
info "║      ├── pg/                 ← Datos PostgreSQL"
info "║      ├── logs/               ← Logs del stack"
info "║      └── nkr-compose.yml     ← Compose de la célula"
info "║"
info "║  Siguiente paso:"
info "║    cd ${CELL_DIR}"
info "║    sudo nkr compose up -d"
info "╚══════════════════════════════════════════════════════════════╝"
