#!/bin/bash
# =============================================================================
# NKR API — Smoke test E2E
# =============================================================================
# Crea un tenant temporal `smoke-test-<random>` en una cell, ejercita todos
# los endpoints del API en orden de dependencia, valida fields del response,
# mide timing per-step, y borra el tenant al final (cleanup garantizado por
# trap EXIT incluso si hay fallo a mitad de camino).
#
# Uso:
#   tools/test-api.sh                    # cobertura básica (~90s)
#   tools/test-api.sh --full             # incluye addons/git + modules/install (~5min)
#   tools/test-api.sh --with-dns         # incluye DNS+cert (consume cuota Let's Encrypt!)
#   tools/test-api.sh -v                 # verbose: muestra request/response crudos
#
# Variables de entorno (override de defaults):
#   NKR_API_TOKEN     Token Bearer. Si falta, lee /etc/nkr/api.env.
#   NKR_API_BASE      Default: http://127.0.0.1:9090
#   CELL              Default: odoo-v19
#   ODOO_VERSION      Default: 19.0
#   WORKERS           Default: 2
#   DNS_TEST_DOMAIN   Para --with-dns. Default: smoke-test-N.tudominio.com (no funciona)
# =============================================================================

set -euo pipefail

# ───── Parsing flags ─────────────────────────────────────────────────────────
FULL=0
WITH_DNS=0
VERBOSE=0
for arg in "$@"; do
    case "$arg" in
        --full) FULL=1 ;;
        --with-dns) WITH_DNS=1 ;;
        -v|--verbose) VERBOSE=1 ;;
        -h|--help)
            sed -n '/^# Uso:/,/^# ===/p' "$0" | grep -v "^# ===" | sed 's/^# \?//'
            exit 0 ;;
        *) echo "Flag desconocido: $arg (usar --help)"; exit 2 ;;
    esac
done

# ───── Defaults + env override ───────────────────────────────────────────────
: "${NKR_API_BASE:=http://127.0.0.1:9090}"
: "${CELL:=odoo-v19}"
: "${ODOO_VERSION:=19.0}"
: "${WORKERS:=2}"
RANDOM_ID=$RANDOM
: "${TEMP_NAME:=smoke-test-$RANDOM_ID}"
: "${DNS_TEST_DOMAIN:=$TEMP_NAME.example.invalid}"

# Token: env var primero, fallback a /etc/nkr/api.env (con o sin sudo).
if [ -z "${NKR_API_TOKEN:-}" ]; then
    if [ -r /etc/nkr/api.env ]; then
        NKR_API_TOKEN=$(grep -E "^NKR_API_TOKEN=" /etc/nkr/api.env | cut -d= -f2- | tr -d '"' | tr -d "'")
    elif sudo -n true 2>/dev/null; then
        NKR_API_TOKEN=$(sudo cat /etc/nkr/api.env 2>/dev/null | grep -E "^NKR_API_TOKEN=" | cut -d= -f2- | tr -d '"' | tr -d "'")
    fi
fi
if [ -z "${NKR_API_TOKEN:-}" ]; then
    echo "ERROR: no encontré NKR_API_TOKEN. Setealo via env var o asegurá lectura de /etc/nkr/api.env"
    exit 2
fi
TOK="Authorization: Bearer $NKR_API_TOKEN"

# ───── Colors ────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    G='\e[32m'; R='\e[31m'; Y='\e[33m'; B='\e[34m'; D='\e[2m'; N='\e[0m'
else
    G=''; R=''; Y=''; B=''; D=''; N=''
fi

# ───── State para cleanup ───────────────────────────────────────────────────
ACTUAL_NKR_NAME=""
DNS_PROVISIONED=0
declare -A TIMING

# ───── Cleanup (trap EXIT) ──────────────────────────────────────────────────
cleanup() {
    local exit_code=$?
    echo
    echo -e "${Y}[CLEANUP]${N} Fase de limpieza..."
    if [ -n "$ACTUAL_NKR_NAME" ]; then
        if [ "$DNS_PROVISIONED" = "1" ]; then
            curl -s -X DELETE -H "$TOK" --max-time 30 \
                "$NKR_API_BASE/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/dns?delete_cert=0" \
                > /dev/null 2>&1 && echo -e "  ${D}DNS borrado${N}" || echo -e "  ${Y}DNS cleanup falló (ignorado)${N}"
        fi
        # Delete tarda 30-90s (drop DB + stop VM + remove dir + dropear cgroup).
        # 180s da margen para tenants con DB grande + I/O lento.
        curl -s -X DELETE -H "$TOK" --max-time 180 \
            "$NKR_API_BASE/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME?drop_db=1" \
            > /dev/null 2>&1 && echo -e "  ${G}Instancia $ACTUAL_NKR_NAME borrada${N}" \
            || echo -e "  ${R}Falla en delete (revisar manualmente: $ACTUAL_NKR_NAME)${N}"
    else
        echo -e "  ${D}(no se llegó a crear instancia, nada para borrar)${N}"
    fi

    echo
    echo -e "${B}══ Resumen timing por step ══${N}"
    for step in "${!TIMING[@]}"; do
        printf "  %-32s %ss\n" "$step" "${TIMING[$step]}"
    done | sort

    if [ $exit_code -eq 0 ]; then
        echo -e "\n${G}╔════════════════════════════════╗${N}"
        echo -e "${G}║   ✅ ALL TESTS PASSED          ║${N}"
        echo -e "${G}╚════════════════════════════════╝${N}"
    else
        echo -e "\n${R}╔════════════════════════════════╗${N}"
        echo -e "${R}║   ❌ TEST FAILED (exit=$exit_code)     ║${N}"
        echo -e "${R}╚════════════════════════════════╝${N}"
    fi
    exit $exit_code
}
trap cleanup EXIT

# ───── Helpers ───────────────────────────────────────────────────────────────
step() {
    local name="$1"; shift
    local start=$(date +%s.%N)
    echo -en "${B}[STEP]${N} $name ... "
    if [ "$VERBOSE" = "1" ]; then echo; fi
    "$@"
    local rc=$?
    local end=$(date +%s.%N)
    local dur=$(awk "BEGIN{printf \"%.2f\", $end - $start}")
    TIMING["$name"]="$dur"
    if [ $rc -eq 0 ]; then
        echo -e "${G}OK${N} ${D}(${dur}s)${N}"
    else
        echo -e "${R}FAIL${N}"
        return $rc
    fi
}

# Wrapper de curl que falla si HTTP >=400, captura body, opcionalmente print verbose
api_call() {
    local method="$1"
    local path="$2"
    local data="${3:-}"
    local maxtime="${4:-30}"

    local args=(-s -f -X "$method" -H "$TOK" --max-time "$maxtime")
    if [ -n "$data" ]; then
        args+=(-H "Content-Type: application/json" -d "$data")
    fi
    args+=("$NKR_API_BASE$path")

    if [ "$VERBOSE" = "1" ]; then
        echo -e "  ${D}→ $method $path${N}" >&2
        if [ -n "$data" ]; then
            echo -e "  ${D}body: $data${N}" >&2
        fi
    fi

    local resp
    resp=$(curl "${args[@]}")
    local rc=$?
    if [ "$VERBOSE" = "1" ] && [ -n "$resp" ]; then
        # Verbose va a stderr para no contaminar el stdout que el caller captura
        echo -e "  ${D}← $(echo "$resp" | head -c 500)${N}" >&2
    fi
    if [ $rc -ne 0 ]; then return $rc; fi
    echo "$resp"
}

# Extrae un campo del JSON. Falla si no existe o es null.
json_get() {
    local key="$1"
    python3 -c "import json,sys; d=json.load(sys.stdin); v=d
for k in '$key'.split('.'):
    v = v[int(k)] if k.isdigit() else v[k]
print(v)"
}

# ============================================================================
# TESTS
# ============================================================================

echo -e "${B}══════════════════════════════════════════════${N}"
echo -e "${B}  NKR API smoke test${N}"
echo -e "${B}══════════════════════════════════════════════${N}"
echo -e "  Base URL:    ${NKR_API_BASE}"
echo -e "  Cell:        ${CELL} (${ODOO_VERSION})"
echo -e "  Workers:     ${WORKERS}"
echo -e "  Tenant:      ${TEMP_NAME}"
echo -e "  Mode:        $([ $FULL = 1 ] && echo full || echo basic)$([ $WITH_DNS = 1 ] && echo ' +dns')"
echo

# ─── 1. Health (no auth) ────────────────────────────────────────────────────
test_health() {
    local resp
    resp=$(curl -s -f --max-time 5 "$NKR_API_BASE/api/v1/health")
    local ok
    ok=$(echo "$resp" | python3 -c "import json,sys;print(json.load(sys.stdin).get('ok'))")
    [ "$ok" = "True" ] || { echo "expected ok=true, got $resp"; return 1; }
}
step "1. GET /health"                        test_health

# ─── 2. Metrics (no auth, Prometheus format) ────────────────────────────────
test_metrics() {
    local resp
    resp=$(curl -s -f --max-time 5 "$NKR_API_BASE/metrics")
    echo "$resp" | grep -q "^# HELP " || { echo "no Prometheus HELP markers"; return 1; }
}
step "2. GET /metrics"                       test_metrics

# ─── 3. List cells ──────────────────────────────────────────────────────────
test_list_cells() {
    local resp
    resp=$(api_call GET "/api/v1/cells" "" 5)
    local found
    found=$(echo "$resp" | python3 -c "
import json,sys
d=json.load(sys.stdin)
cells=[c['name'] for c in d.get('cells', [])]
print('1' if '$CELL' in cells else '0')
")
    [ "$found" = "1" ] || { echo "cell '$CELL' no aparece en lista de cells: $resp"; return 1; }
}
step "3. GET /cells (verifica $CELL existe)"  test_list_cells

# ─── 4. Create instance (con admin_user_password) ───────────────────────────
test_create() {
    local admin_pwd="MasterPwd-$RANDOM_ID-$(date +%s)"
    local user_pwd="UserPwd-$RANDOM_ID-$(date +%s)"
    local body
    body=$(python3 -c "
import json
print(json.dumps({
    'nkr_name': '$TEMP_NAME',
    'mode': 'production',
    'odoo_version': '$ODOO_VERSION',
    'workers': $WORKERS,
    'edition': 'community',
    'admin_passwd': '$admin_pwd',
    'admin_user_password': '$user_pwd',
}))
")
    local resp
    resp=$(api_call POST "/api/v1/cells/$CELL/instances" "$body" 180)
    ACTUAL_NKR_NAME=$(echo "$resp" | json_get nkr_name)
    [ -n "$ACTUAL_NKR_NAME" ] || { echo "no nkr_name en response: $resp"; return 1; }
    local guest_ip
    guest_ip=$(echo "$resp" | json_get guest_ip)
    [ -n "$guest_ip" ] || { echo "no guest_ip"; return 1; }
    echo -e "    ${D}→ Asignado: $ACTUAL_NKR_NAME @ $guest_ip${N}"
    echo "$user_pwd" > /tmp/.nkr-test-user-pwd-$$
}
step "4. POST /instances (mode=production, admin_user_password)" test_create

# ─── 5. Get info ────────────────────────────────────────────────────────────
test_get_info() {
    local resp
    resp=$(api_call GET "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME" "" 10)
    local running
    running=$(echo "$resp" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(d.get('nkr_status', {}).get('running', False))
")
    # No debe estar running aún (admin_user_password no fue mandado en handle_action,
    # solo en POST /instances con seteo si flag se mandó). Acepto ambos estados.
    echo -e "    ${D}→ running=$running${N}"
}
step "5. GET /instances/{name}"              test_get_info

# ─── 6. Polling de port_8069_up (boot completo + admin_user_password aplicada) ──
test_wait_ready() {
    local max=40
    local i=0
    while [ $i -lt $max ]; do
        local resp
        resp=$(api_call GET "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME" "" 5) || return 1
        local up
        up=$(echo "$resp" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(d.get('nkr_status', {}).get('port_8069_up', False))
")
        if [ "$up" = "True" ]; then return 0; fi
        i=$((i+1))
        sleep 3
    done
    echo "tenant no llegó a port_8069_up tras 120s"
    return 1
}
step "6. Polling :8069 ready"                test_wait_ready

# ─── 7. PSQL sandbox (simple SELECT) ────────────────────────────────────────
test_psql() {
    local resp
    resp=$(api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/psql" \
        '{"query":"SELECT 1 AS ok","max_rows":1}' 30)
    # El endpoint psql devuelve {csv, rows_returned, exit_code, ...}, no rows[].
    # exit_code=0 + rows_returned>=1 = SQL ejecutado bien.
    local exit_code rows_returned
    exit_code=$(echo "$resp" | python3 -c "import json,sys;print(json.load(sys.stdin).get('exit_code',-1))")
    rows_returned=$(echo "$resp" | python3 -c "import json,sys;print(json.load(sys.stdin).get('rows_returned',0))")
    [ "$exit_code" = "0" ] || { echo "psql exit_code=$exit_code: $resp"; return 1; }
    [ "$rows_returned" -ge 1 ] || { echo "rows_returned=$rows_returned: $resp"; return 1; }
}
step "7. POST /psql (SELECT 1)"              test_psql

# ─── 8. PATCH config (re-apply workers — idempotente) ───────────────────────
test_patch_config() {
    local resp
    # PATCH workers cambia ram+chrs+limit_memory_*. Por default dispara
    # restart de la VM (~60s). Para el smoke pasamos restart=false:
    # validamos que el endpoint upsertea OK, sin esperar el restart
    # (que el step 10 ya prueba aparte).
    resp=$(api_call PATCH "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/config" \
        "{\"workers\":$WORKERS,\"restart\":false}" 30)
    echo "$resp" | grep -q applied || { echo "patch no devolvió applied: $resp"; return 1; }
}
step "8. PATCH /config (workers=$WORKERS)"   test_patch_config

# ─── 9. Logs tail ───────────────────────────────────────────────────────────
test_logs() {
    local resp
    resp=$(api_call GET "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/logs?tail=10" "" 10)
    local size
    size=$(echo "$resp" | python3 -c "
import json,sys
d=json.load(sys.stdin)
print(d.get('file_size', 0))
")
    [ "$size" -gt 0 ] || { echo "file_size=0, ¿odoo no escribió log?"; return 1; }
    echo -e "    ${D}→ file_size=$size bytes${N}"
}
step "9. GET /logs?tail=10"                  test_logs

# ─── 10. Restart action ─────────────────────────────────────────────────────
test_restart() {
    local resp
    # Restart = stop + start. Boot del Odoo (carga registry + workers) ~30-45s.
    # 120s da margen.
    resp=$(api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/actions" \
        '{"action":"restart"}' 120)
    echo "$resp" | grep -q '"action"' || { echo "no action in response: $resp"; return 1; }
    sleep 3
}
step "10. POST /actions {restart}"           test_restart

# ─── 11. Cache purge (global) ───────────────────────────────────────────────
test_cache_purge() {
    local resp
    resp=$(api_call POST "/api/v1/admin/cache/purge" "" 10)
    echo "$resp" | grep -q purged || { echo "no purged field: $resp"; return 1; }
    local n
    n=$(echo "$resp" | python3 -c "import json,sys;print(json.load(sys.stdin).get('purged',-1))")
    [ "$n" -ge 0 ] || { echo "purged inválido: $n"; return 1; }
    echo -e "    ${D}→ purged=$n entries${N}"
}
step "11. POST /admin/cache/purge"           test_cache_purge

# ─── 12. (--full only) addons/git con repo OCA chico ────────────────────────
if [ "$FULL" = "1" ]; then
    test_addons_git() {
        local resp
        resp=$(api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/addons/git" \
            "{\"repo_url\":\"https://github.com/OCA/queue.git\",\"ref\":\"$ODOO_VERSION\"}" 180)
        local count
        count=$(echo "$resp" | json_get module_count)
        [ "$count" -gt 0 ] || { echo "module_count=$count: $resp"; return 1; }
        echo -e "    ${D}→ módulos explotados: $count${N}"
    }
    step "12. POST /addons/git (OCA/queue)"  test_addons_git

    # Restart para que Odoo reescanee manifests
    test_restart_after_git() {
        api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/actions" \
            '{"action":"restart"}' 60 > /dev/null
        sleep 5
        # Wait again
        test_wait_ready
    }
    step "13. Restart + ready (post-git)"   test_restart_after_git
fi

# ─── 14. (--with-dns only) DNS provisioning ─────────────────────────────────
if [ "$WITH_DNS" = "1" ]; then
    test_dns() {
        local resp
        resp=$(api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/dns" \
            "{\"dns\":\"$DNS_TEST_DOMAIN\",\"enable_websocket\":true}" 200)
        echo "$resp" | grep -q vhost_path || return 1
        DNS_PROVISIONED=1
    }
    step "14. POST /dns ($DNS_TEST_DOMAIN)" test_dns
fi

# ─── 15. Stop ───────────────────────────────────────────────────────────────
test_stop() {
    local resp
    resp=$(api_call POST "/api/v1/cells/$CELL/instances/$ACTUAL_NKR_NAME/actions" \
        '{"action":"stop"}' 30)
    echo "$resp" | grep -q '"action"' || { echo "no action in response: $resp"; return 1; }
}
step "15. POST /actions {stop}"              test_stop

# Cleanup file con la password del user
rm -f /tmp/.nkr-test-user-pwd-$$ 2>/dev/null || true

# Trap EXIT manda al cleanup → DELETE instance + DNS si fue provisionado.
