#!/bin/bash
# =============================================================================
# NKR Multi-Tenant — Provisionar cliente(s)
# =============================================================================
# Crea disco (CoW si es posible), config Odoo y config nginx para un cliente.
#
# Uso:
#   sudo ./deploy/mt-provision.sh                # Provisionar TODOS los clientes
#   sudo ./deploy/mt-provision.sh cliente1        # Provisionar solo uno
#   sudo ./deploy/mt-provision.sh --base          # Solo crear disco base
# =============================================================================

set -e
source "$(dirname "$0")/mt-common.sh"
check_root
check_binary
check_clients

DISK_DIR=$(yaml_global "disk_dir")
CONFIG_DIR=$(yaml_global "config_dir")
DATA_DIR=$(yaml_global "data_dir")
MODULES_DIR=$(yaml_global "modules_dir")
BASE_DISK=$(yaml_global "base_disk")
NGINX_SITES=$(yaml_global "nginx_sites_dir")
NGINX_ENABLED=$(yaml_global "nginx_enabled_dir")
PG_USER=$(yaml_global "pg_user")
PG_PASSWORD=$(yaml_global "pg_password")
ADMIN_PASSWD=$(yaml_global "odoo_admin_passwd")
DISK_SIZE_GB=$(yaml_global "odoo_disk_size_gb")

# ── Crear directorios base ──
mkdir -p "$DISK_DIR" "$CONFIG_DIR" "$DATA_DIR/pg" "$MODULES_DIR"

# =============================================================================
# Paso 0: Disco base Odoo (se crea una sola vez)
# =============================================================================

create_base_disk() {
    if [[ -f "$BASE_DISK" ]]; then
        info "Disco base ya existe: $BASE_DISK ($(du -h "$BASE_DISK" | cut -f1))"
        return 0
    fi

    info "╔══════════════════════════════════════════════════════════════╗"
    info "║  Creando disco base Odoo (una sola vez)                    ║"
    info "╚══════════════════════════════════════════════════════════════╝"

    if [[ -f "$NKR_DIR/Nkrfile.odoo" ]]; then
        info "Construyendo desde Nkrfile.odoo..."
        "$NKR_BIN" build -f "$NKR_DIR/Nkrfile.odoo" -o "$BASE_DISK" --size-gb "${DISK_SIZE_GB:-4}" --context "$NKR_DIR"
    else
        info "Descargando imagen Odoo 17 desde Docker Hub..."
        "$NKR_BIN" pull odoo:17.0 "$BASE_DISK" --size-gb "${DISK_SIZE_GB:-4}"
    fi

    info "Disco base creado: $BASE_DISK ($(du -h "$BASE_DISK" | cut -f1))"
}

# =============================================================================
# Provisionar un cliente individual
# =============================================================================

provision_client() {
    local client_name="$1"
    IFS='|' read -r name domain db_name ram chrs stmt_timeout conn_limit <<< "$(get_client "$client_name")"

    if [[ -z "$name" ]]; then
        error "Cliente '$client_name' no encontrado en $CLIENTS_YML"
    fi

    local vm_id=$(get_vm_id "$name")
    local ip=$(vm_ip "$vm_id")
    local disk=$(client_disk "$name")
    local config=$(client_config "$name")
    local data=$(client_data "$name")

    info "── Provisionando: $name (id=$vm_id, IP=$ip) ──"

    # ── 1. Disco (Copy-on-Write desde base) ──
    if [[ -f "$disk" ]]; then
        warn "  Disco ya existe: $disk (omitiendo)"
    else
        info "  Creando disco desde base (CoW si disponible)..."
        cp --reflink=auto "$BASE_DISK" "$disk"
        local disk_size=$(du -h "$disk" | cut -f1)
        # Verificar si fue CoW real (tamaño en disco mucho menor)
        local disk_apparent=$(du -h --apparent-size "$disk" | cut -f1)
        local disk_actual=$(du -h "$disk" | cut -f1)
        info "  Disco: $disk (aparente: $disk_apparent, en disco: $disk_actual)"
    fi

    # ── 2. Directorio de datos ──
    mkdir -p "$data/filestore"

    # ── 3. Config Odoo personalizada ──
    if [[ -f "$config" ]]; then
        warn "  Config ya existe: $config (omitiendo)"
    else
        info "  Generando config Odoo..."
        cat > "$config" << ODOOCONF
[options]
; =============================================================================
; $name — Configuración Odoo para NKR (auto-generada)
; =============================================================================
; Dominio: $domain
; DB: $db_name
; VM ID: $vm_id | IP: $ip
; =============================================================================

addons_path = /usr/lib/python3/dist-packages/odoo/addons,/mnt/extra-addons
data_dir = /var/lib/odoo

; ── Base de datos (PostgreSQL compartido en 10.0.0.2) ──
db_host = 10.0.0.2
db_port = 5432
db_user = $PG_USER
db_password = $PG_PASSWORD
admin_passwd = $ADMIN_PASSWD

; ── Filtro DB: solo esta base de datos ──
db_maxconn = 16
db_name = $db_name
db_template = template1
dbfilter = ^${db_name}\$
list_db = False

; ── Logs ──
log_handler = [':INFO']
log_level = info
logfile = None

; ── Performance (ajustado para multi-tenant, RAM mínima) ──
max_cron_threads = 1
workers = 0
limit_memory_hard = 536870912
limit_memory_soft = 419430400
limit_time_cpu = 60
limit_time_real = 120

; ── HTTP ──
xmlrpc = True
xmlrpc_interface =
xmlrpc_port = 8069

csv_internal_sep = ,
ODOOCONF
        info "  Config: $config"
    fi

    # ── 4. Config nginx ──
    if [[ -d "$NGINX_SITES" ]]; then
        local nginx_conf="$NGINX_SITES/nkr-${name}"
        if [[ -f "$nginx_conf" ]]; then
            warn "  Nginx config ya existe: $nginx_conf (omitiendo)"
        else
            info "  Generando nginx config..."
            cat > "$nginx_conf" << NGINXCONF
# NKR auto-generated — $name
# Dominio: $domain → $ip:8069
upstream nkr_${name} {
    server ${ip}:8069;
}

server {
    listen 80;
    server_name ${domain};

    # Certbot challenge
    location /.well-known/acme-challenge/ {
        root /var/www/certbot;
    }

    # Redirect to HTTPS (descomentar tras obtener certificado)
    # location / {
    #     return 301 https://\$host\$request_uri;
    # }

    # HTTP directo (usar mientras no haya SSL)
    location / {
        proxy_pass http://nkr_${name};
        proxy_set_header Host \$host;
        proxy_set_header X-Real-IP \$remote_addr;
        proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto \$scheme;
        proxy_read_timeout 720s;
        proxy_connect_timeout 60s;
        client_max_body_size 100m;
    }

    # WebSocket / longpolling
    location /websocket {
        proxy_pass http://nkr_${name};
        proxy_set_header Upgrade \$http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_set_header Host \$host;
        proxy_read_timeout 86400;
    }
}

# HTTPS (descomentar tras: sudo certbot --nginx -d ${domain})
# server {
#     listen 443 ssl http2;
#     server_name ${domain};
#
#     ssl_certificate /etc/letsencrypt/live/${domain}/fullchain.pem;
#     ssl_certificate_key /etc/letsencrypt/live/${domain}/privkey.pem;
#
#     location / {
#         proxy_pass http://nkr_${name};
#         proxy_set_header Host \$host;
#         proxy_set_header X-Real-IP \$remote_addr;
#         proxy_set_header X-Forwarded-For \$proxy_add_x_forwarded_for;
#         proxy_set_header X-Forwarded-Proto https;
#         proxy_read_timeout 720s;
#         proxy_connect_timeout 60s;
#         client_max_body_size 100m;
#     }
#
#     location /websocket {
#         proxy_pass http://nkr_${name};
#         proxy_set_header Upgrade \$http_upgrade;
#         proxy_set_header Connection "upgrade";
#         proxy_set_header Host \$host;
#         proxy_read_timeout 86400;
#     }
# }
NGINXCONF

            # Activar site
            if [[ -d "$NGINX_ENABLED" ]]; then
                ln -sf "$nginx_conf" "$NGINX_ENABLED/nkr-${name}"
            fi
            info "  Nginx: $nginx_conf (activado)"
        fi
    else
        warn "  $NGINX_SITES no existe — nginx no instalado, omitiendo config"
    fi

    # ── 5. Límites PostgreSQL por base de datos ──
    info "  Aplicando límites DB multi-tenant..."
    inject_db_limits "$db_name" "${stmt_timeout:-60000}" "${conn_limit:-10}"

    info "  ✓ $name provisionado"
}

# =============================================================================
# Main
# =============================================================================

if [[ "$1" == "--base" ]]; then
    create_base_disk
    exit 0
fi

info "╔══════════════════════════════════════════════════════════════╗"
info "║  NKR Multi-Tenant — Provisionar Clientes                   ║"
info "╚══════════════════════════════════════════════════════════════╝"

# Asegurar disco base
create_base_disk

if [[ -n "$1" ]]; then
    # Provisionar un solo cliente
    provision_client "$1"
else
    # Provisionar todos
    total=$(count_clients)
    info "Provisionando $total cliente(s)..."
    echo ""

    while IFS='|' read -r name domain db_name ram chrs stmt_timeout conn_limit; do
        provision_client "$name"
        echo ""
    done <<< "$(parse_clients)"
fi

# Recargar nginx si está instalado
if command -v nginx &>/dev/null; then
    if nginx -t 2>/dev/null; then
        nginx -s reload 2>/dev/null || true
        info "nginx recargado"
    else
        warn "nginx config test falló — revisar configs manualmente"
    fi
fi

info "╔══════════════════════════════════════════════════════════════╗"
info "║  Provisión completada                                      ║"
info "╠══════════════════════════════════════════════════════════════╣"
print_client_table
info "║  Siguiente paso:                                           ║"
info "║    sudo ./deploy/mt-compose-gen.sh    (generar compose)    ║"
info "║    sudo nkr compose up -f nkr-compose.yml -d              ║"
info "╚══════════════════════════════════════════════════════════════╝"
