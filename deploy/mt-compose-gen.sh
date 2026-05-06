#!/bin/bash
# =============================================================================
# NKR Multi-Tenant — Generar nkr-compose.yml desde clients.yml
# =============================================================================
# Asigna IDs y puertos de forma determinista:
#   PostgreSQL → id=1, sin puertos externos
#   Clientes   → id=2,3,... en orden de aparición en clients.yml
#   Puertos    → cliente N (offset=N-1): 8069+offset:8069, 8072+offset:8072
#
# IMPORTANTE: El orden de clients.yml determina la asignación de puertos.
#             Añadir clientes al final no altera los puertos de los existentes.
#             Insertar un cliente en el medio SÍ desplaza puertos posteriores.
#
# Uso:
#   sudo ./deploy/mt-compose-gen.sh              # Genera nkr-compose.yml
#   sudo ./deploy/mt-compose-gen.sh --dry-run    # Solo imprime, no escribe
# =============================================================================

set -e
source "$(dirname "$0")/mt-common.sh"
check_root
check_clients

DRY_RUN=false
[[ "$1" == "--dry-run" ]] && DRY_RUN=true

KERNEL=$(yaml_global "kernel")
PG_INITRAMFS=$(yaml_global "pg_initramfs")
ODOO_INITRAMFS=$(yaml_global "odoo_initramfs")
PG_RAM=$(yaml_global "pg_ram")
PG_CHRS=$(yaml_global "pg_chrs")
DISK_DIR=$(yaml_global "disk_dir")
DATA_DIR=$(yaml_global "data_dir")
CONFIG_DIR=$(yaml_global "config_dir")
MODULES_DIR=$(yaml_global "modules_dir")
PG_USER=$(yaml_global "pg_user")
PG_PASSWORD=$(yaml_global "pg_password")

OUTPUT="$NKR_DIR/nkr-compose.yml"
TMPFILE=$(mktemp)

# =============================================================================
# Cabecera
# =============================================================================

cat > "$TMPFILE" << HEADER
# =============================================================================
# nkr-compose.yml — Auto-generado por deploy/mt-compose-gen.sh
# NO editar manualmente — regenerar con: sudo ./deploy/mt-compose-gen.sh
# Generado: $(date -u +"%Y-%m-%dT%H:%M:%SZ")
# =============================================================================

services:

  # ---------------------------------------------------------------------------
  # PostgreSQL compartido — vm_id=1, IP=10.0.0.2
  # ---------------------------------------------------------------------------
  postgresql:
    id: 1
    name: postgresql
    kernel: ${KERNEL}
    initramfs: ${PG_INITRAMFS}
    disks:
      - ${DISK_DIR}/postgres.ext4
    ram: ${PG_RAM}
    chrs: ${PG_CHRS}
    volumes:
      - ${DATA_DIR}/pg:/var/lib/postgresql/data:rw
    env:
      - POSTGRES_USER=${PG_USER}
      - POSTGRES_PASSWORD=${PG_PASSWORD}
    ports: []

HEADER

# =============================================================================
# Clientes Odoo
# =============================================================================

idx=0
while IFS='|' read -r name domain db_name ram chrs; do
    idx=$((idx + 1))
    vm_id=$((idx + 1))
    # Puertos: offset = idx-1 → cliente1 en 8069, cliente2 en 8070, etc.
    offset=$((idx - 1))
    port_http=$((8069 + offset))
    port_poll=$((8072 + offset))
    ip="10.0.0.$((vm_id + 1))"

    cat >> "$TMPFILE" << SVCBLOCK
  # ---------------------------------------------------------------------------
  # ${name} — vm_id=${vm_id}, IP=${ip}
  # Dominio: ${domain}
  # ---------------------------------------------------------------------------
  ${name}:
    id: ${vm_id}
    name: ${name}
    kernel: ${KERNEL}
    initramfs: ${ODOO_INITRAMFS}
    disks:
      - ${DISK_DIR}/${name}.ext4
    ram: ${ram}
    chrs: ${chrs}
    volumes:
      - ${CONFIG_DIR}/${name}.conf:/etc/odoo/odoo.conf
      - ${DATA_DIR}/${name}/filestore:/var/lib/odoo/filestore:rw
      - ${MODULES_DIR}:/mnt/extra-addons
    ports:
      - "${port_http}:8069"
      - "${port_poll}:8072"

SVCBLOCK

done <<< "$(parse_clients)"

# =============================================================================
# Escribir o imprimir
# =============================================================================

LINE_COUNT=$(wc -l < "$TMPFILE")

if $DRY_RUN; then
    info "=== DRY RUN — nkr-compose.yml (${LINE_COUNT} líneas, ${idx} cliente(s) + PostgreSQL) ==="
    cat "$TMPFILE"
    rm -f "$TMPFILE"
    exit 0
fi

mv "$TMPFILE" "$OUTPUT"

info "╔══════════════════════════════════════════════════════════════╗"
info "║  nkr-compose.yml generado correctamente                    ║"
info "╠══════════════════════════════════════════════════════════════╣"
info "  Archivo: $OUTPUT"
info "  Clientes: ${idx} + PostgreSQL = $((idx + 1)) servicios"
info "  Líneas: ${LINE_COUNT}"
echo ""
printf "  %-6s %-20s %-15s %-12s\n" "ID" "NOMBRE" "IP" "PUERTOS HTTP"
printf "  %-6s %-20s %-15s %-12s\n" "──" "──────" "──" "────────────"
printf "  %-6s %-20s %-15s %-12s\n" "1" "postgresql" "10.0.0.2" "(interno)"

idx2=0
while IFS='|' read -r name domain db_name ram chrs; do
    idx2=$((idx2 + 1))
    vm_id2=$((idx2 + 1))
    offset2=$((idx2 - 1))
    port_h=$((8069 + offset2))
    printf "  %-6s %-20s %-15s %-12s\n" "${vm_id2}" "${name}" "10.0.0.$((vm_id2 + 1))" "${port_h}:8069"
done <<< "$(parse_clients)"

echo ""
info "╠══════════════════════════════════════════════════════════════╣"
info "║  Siguiente paso:                                           ║"
info "║    sudo nkr compose up -f nkr-compose.yml -d              ║"
info "╚══════════════════════════════════════════════════════════════╝"
