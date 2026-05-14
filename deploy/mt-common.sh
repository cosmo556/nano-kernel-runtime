#!/bin/bash
# =============================================================================
# NKR Multi-Tenant — Funciones comunes para parsear clients.yml
# =============================================================================
# Uso: source deploy/mt-common.sh
# =============================================================================

DEPLOY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NKR_DIR="$(cd "$DEPLOY_DIR/.." && pwd)"
NKR_BIN="$NKR_DIR/target/release/nkr"
CLIENTS_YML="$DEPLOY_DIR/clients.yml"

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; CYAN='\033[0;36m'; NC='\033[0m'
info()  { echo -e "${GREEN}[NKR-MT]${NC} $1"; }
warn()  { echo -e "${YELLOW}[NKR-MT]${NC} $1"; }
error() { echo -e "${RED}[NKR-MT]${NC} $1"; exit 1; }
timer() { echo -e "${CYAN}[NKR-MT]${NC} $1"; }

# ── Validaciones ──
check_root()    { [[ $EUID -ne 0 ]] && error "Ejecutar con sudo"; }
check_binary()  { [[ ! -f "$NKR_BIN" ]] && error "Binario NKR no encontrado. Ejecuta: cargo build --release"; }
check_clients() { [[ ! -f "$CLIENTS_YML" ]] && error "No se encontró $CLIENTS_YML"; }

# ── Leer valor global del YAML ──
# Uso: val=$(yaml_global "pg_ram")
yaml_global() {
    local key="$1"
    awk -v k="$key" '
        /^global:/ { in_global=1; next }
        /^[a-z]/ && !/^  / { in_global=0 }
        in_global && $1 == k":" { print $2; exit }
    ' "$CLIENTS_YML"
}

# ── Leer lista de clientes ──
# Retorna líneas con formato: name|domain|db_name|ram|chrs|stmt_timeout|conn_limit
# Campos opcionales usan defaults globales si no están definidos en el cliente
parse_clients() {
    local default_ram=$(yaml_global "odoo_ram")
    local default_chrs=$(yaml_global "odoo_chrs")
    local default_stmt=$(yaml_global "db_statement_timeout")
    local default_conn=$(yaml_global "db_conn_limit")
    # Valores de seguridad si no están definidos en clients.yml aún
    [[ -z "$default_stmt" ]] && default_stmt=60000
    [[ -z "$default_conn" ]] && default_conn=10

    awk -v def_ram="$default_ram" -v def_chrs="$default_chrs" \
        -v def_stmt="$default_stmt" -v def_conn="$default_conn" '
        /^clients:/ { in_clients=1; next }
        /^[a-z]/ && !/^  / && !/^  -/ { in_clients=0 }
        !in_clients { next }

        /^  - name:/ {
            if (name != "") {
                ram   = (client_ram   != "") ? client_ram   : def_ram
                chrs  = (client_chrs  != "") ? client_chrs  : def_chrs
                stmt  = (client_stmt  != "") ? client_stmt  : def_stmt
                conn  = (client_conn  != "") ? client_conn  : def_conn
                print name "|" domain "|" db_name "|" ram "|" chrs "|" stmt "|" conn
            }
            name = $3; domain = ""; db_name = ""
            client_ram = ""; client_chrs = ""; client_stmt = ""; client_conn = ""
            next
        }
        /^    domain:/              { domain     = $2; next }
        /^    db_name:/             { db_name    = $2; next }
        /^    ram:/                 { client_ram  = $2; next }
        /^    chrs:/                { client_chrs = $2; next }
        /^    db_statement_timeout:/ { client_stmt = $2; next }
        /^    db_conn_limit:/       { client_conn = $2; next }

        END {
            if (name != "") {
                ram   = (client_ram   != "") ? client_ram   : def_ram
                chrs  = (client_chrs  != "") ? client_chrs  : def_chrs
                stmt  = (client_stmt  != "") ? client_stmt  : def_stmt
                conn  = (client_conn  != "") ? client_conn  : def_conn
                print name "|" domain "|" db_name "|" ram "|" chrs "|" stmt "|" conn
            }
        }
    ' "$CLIENTS_YML"
}

# ── Contar clientes ──
count_clients() {
    parse_clients | wc -l
}

# ── Obtener datos de un cliente por nombre ──
# Uso: IFS='|' read -r name domain db_name ram chrs stmt_timeout conn_limit <<< "$(get_client cliente1)"
get_client() {
    parse_clients | grep "^$1|"
}

# ── Calcular vm_id para un cliente (2 + posición en lista) ──
# vm_id=1 reservado para PostgreSQL
get_vm_id() {
    local client_name="$1"
    local idx=0
    while IFS='|' read -r name domain db_name ram chrs; do
        idx=$((idx + 1))
        if [[ "$name" == "$client_name" ]]; then
            echo $((idx + 1))  # +1 porque id=1 es PG
            return 0
        fi
    done <<< "$(parse_clients)"
    echo "0"
    return 1
}

# ── IP del guest dado vm_id ──
vm_ip() { echo "10.0.0.$(($1 + 1))"; }

# ── Aplicar límites multi-tenant a una base de datos en PostgreSQL ──
# Uso: inject_db_limits <db_name> <stmt_timeout_ms> <conn_limit>
# Requiere que la VM PostgreSQL (10.0.0.2:5432) esté corriendo.
inject_db_limits() {
    local db_name="$1"
    local stmt_timeout="${2:-60000}"
    local conn_limit="${3:-10}"
    local pg_host="10.0.0.2"
    local pg_port="5432"
    local pg_user
    pg_user=$(yaml_global "pg_user")
    local pg_pass
    pg_pass=$(yaml_global "pg_password")

    # Esperar a que PostgreSQL esté listo (máx 30 intentos × 1s)
    local attempts=0
    while ! PGPASSWORD="$pg_pass" pg_isready -h "$pg_host" -p "$pg_port" \
              -U "$pg_user" -q 2>/dev/null; do
        attempts=$((attempts + 1))
        if [[ $attempts -ge 30 ]]; then
            warn "  PostgreSQL no disponible en ${pg_host}:${pg_port} — omitiendo límites para $db_name"
            return 0
        fi
        sleep 1
    done

    PGPASSWORD="$pg_pass" psql -h "$pg_host" -p "$pg_port" -U "$pg_user" \
        -d postgres -q \
        -c "ALTER DATABASE \"${db_name}\" SET statement_timeout = '${stmt_timeout}ms';" \
        -c "ALTER DATABASE \"${db_name}\" CONNECTION LIMIT ${conn_limit};" \
        2>/dev/null \
    && info "  DB límites aplicados: timeout=${stmt_timeout}ms, conexiones=${conn_limit}" \
    || warn "  No se pudieron aplicar límites DB para $db_name (¿aún no existe? se reintentará en la próxima provisión)"
}

# ── Rutas por cliente ──
client_disk()   { echo "$(yaml_global disk_dir)/${1}.ext4"; }
client_config() { echo "$(yaml_global config_dir)/${1}.conf"; }
client_data()   { echo "$(yaml_global data_dir)/${1}"; }

# ── Resumen del estado ──
print_client_table() {
    local idx=0
    echo ""
    printf "  %-4s %-20s %-30s %-10s %-6s %-6s %-15s\n" "ID" "NOMBRE" "DOMINIO" "DB" "RAM" "CHRs" "IP"
    printf "  %-4s %-20s %-30s %-10s %-6s %-6s %-15s\n" "──" "──────" "───────" "──" "───" "────" "──"
    printf "  %-4s %-20s %-30s %-10s %-6s %-6s %-15s\n" "1" "postgresql" "(interno)" "—" "$(yaml_global pg_ram)" "$(yaml_global pg_chrs)" "10.0.0.2"

    while IFS='|' read -r name domain db_name ram chrs; do
        idx=$((idx + 1))
        local vm_id=$((idx + 1))
        printf "  %-4s %-20s %-30s %-10s %-6s %-6s %-15s\n" "$vm_id" "$name" "$domain" "$db_name" "${ram}" "${chrs}" "$(vm_ip $vm_id)"
    done <<< "$(parse_clients)"
    echo ""
}
