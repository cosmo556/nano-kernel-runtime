// =============================================================================
// nkr-api-server — Unprivileged HTTP proxy to the root nkr daemon
// =============================================================================
//
// Runs as a dedicated unprivileged user (nkr-api) and exposes the panel API
// over HTTP. Translates each HTTP request into an IpcRequest over the UDS
// exposed by the root daemon at /var/run/nkr.sock.
//
// The proxy does:
//   - HTTP framing (read method/path/body, write status/headers/body)
//   - Bearer token auth (NKR_API_TOKEN)
//   - Identifier validation (defense in depth; daemon re-validates)
//   - Body size limits (64 KiB for POST /instances, 1 KiB for actions)
//   - Bounded concurrency (MAX_INFLIGHT threads)
//
// The proxy does NOT:
//   - Touch any NKR business logic (cell creation, VM spawn, DB drop)
//   - Open /proc or /sys
//   - Require root, SYS_ADMIN, or any capabilities
//
// TLS: terminate in nginx/caddy in front of this binary. Let's Encrypt wildcard
// certs already exist for the Odoo edge — reuse the same nginx:
//
//     server {
//       listen 443 ssl http2;
//       server_name nkr.yourdomain.com;
//       ssl_certificate     /etc/letsencrypt/live/yourdomain.com/fullchain.pem;
//       ssl_certificate_key /etc/letsencrypt/live/yourdomain.com/privkey.pem;
//       location / {
//         proxy_pass http://127.0.0.1:9090;
//         proxy_http_version 1.1;
//         proxy_set_header Host $host;
//         proxy_read_timeout 120s;
//       }
//     }
// =============================================================================

#[path = "../ipc.rs"]
mod ipc;

#[path = "../api_http.rs"]
mod api_http;

use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use api_http::{
    check_auth, is_safe_addons_path, is_safe_dns, is_safe_git_ref, is_safe_git_url,
    is_safe_identifier, parse_headers, parse_request_line, query_get, read_request,
    HttpResponse, GIT_BODY_LIMIT, PYLIBS_BODY_LIMIT,
};
use ipc::{IpcRequest, IpcResponse};

const MAX_INFLIGHT: u32 = 64;
const CREATE_BODY_LIMIT: usize = 64 * 1024;
const ACTION_BODY_LIMIT: usize = 1024;

// Filesystem root managed by the proxy (addons, enterprise, pylibs live here).
const NKR_ROOT: &str = "/mnt/nkr";

// Timeouts for git / pip subprocesses (wall-clock seconds).
const GIT_TIMEOUT_S: u64 = 600;
const PIP_TIMEOUT_S: u64 = 300;

struct InflightGuard {
    counter: Arc<AtomicU32>,
}
impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::Relaxed);
    }
}

fn main() {
    let bind_host = std::env::var("NKR_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = std::env::var("NKR_API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9090);
    let addr = format!("{}:{}", bind_host, port);

    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[NKR-API-SERVER] bind {} falló: {}", addr, e);
            std::process::exit(1);
        }
    };
    eprintln!(
        "[NKR-API-SERVER] Escuchando HTTP en http://{} → UDS {}",
        addr,
        ipc::socket_path().display()
    );
    if bind_host == "0.0.0.0" {
        eprintln!(
            "[NKR-API-SERVER] WARN: 0.0.0.0 expone HTTP plano a la red. \
             Poner nginx/caddy delante con TLS, o bindear a 127.0.0.1."
        );
    }

    let inflight = Arc::new(AtomicU32::new(0));
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };

        if inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT {
            let body = r#"{"error":"server_busy","retry_after":"1s"}"#;
            let resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            let _ = stream.write_all(resp.as_bytes());
            continue;
        }
        inflight.fetch_add(1, Ordering::Relaxed);
        let guard_counter = Arc::clone(&inflight);

        std::thread::spawn(move || {
            // Cut slowloris: a client that opens the connection and never
            // sends bytes (or sends them byte-by-byte) would hold an
            // MAX_INFLIGHT slot indefinitely. 15s is enough for a healthy
            // panel and kills any stuck handshake.
            let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
            let _guard = InflightGuard {
                counter: guard_counter,
            };
            let read_stream = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            };
            let (headers, body) = match read_request(read_stream) {
                Some(x) => x,
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            };
            let (method, path, query) = match parse_request_line(&headers) {
                Some(x) => x,
                None => {
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    return;
                }
            };
            let parsed_headers = parse_headers(&headers);

            let resp = route(method, path, query, &parsed_headers, &body);
            let _ = stream.write_all(resp.to_wire().as_bytes());
        });
    }
}

// =============================================================================
// HTTP → IPC routing
// =============================================================================

fn route(
    method: &str,
    path: &str,
    query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> HttpResponse {
    // /api/v1/health is exempt from auth (used by load balancers).
    if method == "GET" && path == "/api/v1/health" {
        return ipc_to_http(ipc_call(&IpcRequest::Health));
    }
    // /metrics also exempt — Prometheus scrapers usually don't send a token.
    if method == "GET" && path == "/metrics" {
        return ipc_to_http(ipc_call(&IpcRequest::RenderMetrics));
    }

    if let Err(resp) = check_auth(headers) {
        return resp;
    }

    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method, segs.as_slice()) {
        ("GET", ["api", "v1", "cells"]) => ipc_to_http(ipc_call(&IpcRequest::ListCells)),

        ("POST", ["api", "v1", "instances"]) => handle_create_http(None, body),

        ("POST", ["api", "v1", "admin", "cache", "purge"]) => {
            ipc_to_http(ipc_call(&IpcRequest::PurgeCache))
        }

        ("GET", ["api", "v1", "cells", cell, "enterprise"]) => {
            if !is_safe_identifier(cell) {
                return HttpResponse::error(400, "invalid_cell", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::GetEnterpriseStatus {
                cell: (*cell).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", cell, "instances"]) => handle_create_http(Some(cell), body),

        ("GET", ["api", "v1", "cells", _cell, "instances", name]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::GetInfo {
                nkr_name: (*name).to_string(),
            }))
        }

        ("GET", ["api", "v1", "cells", cell, "instances", name, "create-status"]) => {
            // Estado de un create asíncrono (POST /instances → 202). El panel
            // pollea esto hasta status ∈ {ready, failed}. Ver NKR_API.md §4.4.1.
            if !is_safe_identifier(cell) {
                return HttpResponse::error(400, "invalid_cell", None);
            }
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::GetCreateStatus {
                cell: (*cell).to_string(),
                nkr_name: (*name).to_string(),
            }))
        }

        ("DELETE", ["api", "v1", "cells", _cell, "instances", name]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            let drop_db = query_get(query, "drop_db").as_deref() != Some("0");
            ipc_to_http(ipc_call(&IpcRequest::DeleteInstance {
                nkr_name: (*name).to_string(),
                drop_db,
            }))
        }

        ("GET", ["api", "v1", "cells", _cell, "instances", name, "metrics"]) => {
            // Snapshot de métricas de UNA instancia (JSON) — para la pestaña
            // "Métricas" del panel. El daemon cachea ~30s por VM (el `du` de
            // disco más, ~5min), así que pollear seguido es gratis: la caché es
            // el rate-limit, no devuelve 429. Recomendado: el panel pollea cada
            // 30-60s mientras la pestaña esté abierta. Ver NKR_API.md §4.1.2.
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::MetricsForVm {
                nkr_name: (*name).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "reload"]) => {
            // Reload de workers Odoo SIN reiniciar la VM. ~3s, sin downtime
            // del master ni VM. Auto-disparado por addons/git tras clone OK
            // (back-compat: el panel puede llamarlo explícitamente con esto).
            // Idempotente. Más rápido que /actions {restart} (~13-25s) y más
            // confiable que /modules/upgrade en multi-worker (que solo refresca
            // el worker que procesa el request). Ver NKR_API.md §4.18.
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::ReloadWorkers {
                nkr_name: (*name).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "sso"]) => {
            // SSO one-shot: pre-auth admin via JSON-RPC interno y devuelve
            // URL firmada HMAC. El admin_passwd jamás sale del host (sólo
            // el session_id viaja, opaco). Body opcional `{"user":"admin"}`.
            // TTL 30s. Ver NKR_API.md §4.20 y nkr_sso.md (módulo Odoo).
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            let mut user = "admin".to_string();
            if !body.is_empty() {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
                    if let Some(u) = v.get("user").and_then(|x| x.as_str()) {
                        user = u.to_string();
                    }
                }
            }
            ipc_to_http(ipc_call(&IpcRequest::Sso {
                nkr_name: (*name).to_string(),
                user,
            }))
        }

        ("GET", ["api", "v1", "cells", _cell, "instances", name, "diag"]) |
        ("POST", ["api", "v1", "cells", _cell, "instances", name, "diag"]) => {
            // Diagnóstico HOST-side: dump de stacks/wchan/cpu de los threads
            // del proceso `nkr` del tenant. Útil durante un cuelgue para
            // capturar evidencia antes que el watchdog dispare restart auto.
            // Acepta GET (idempotente, sólo lectura de /proc) y POST (por
            // consistencia con los otros endpoints de actions).
            // Output: text/plain. Ver §X.X de NKR_API.md.
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            ipc_to_http(ipc_call(&IpcRequest::Diag {
                nkr_name: (*name).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "balloon"]) => {
            // Marca la VM como ACTIVE en el ballooning dinámico. Idempotente:
            // múltiples calls renuevan el TS sin re-aplicar config_change.
            // Body opcional `{"state": "active"}` (default). IDLE no se setea
            // explícitamente — viene por decay tras `balloon_decay_secs` sin
            // renovación. Si la VM tiene balloon estático (PROD por tier),
            // la señal se entrega pero el state machine la ignora — devolvemos
            // 202 igual para que el panel no necesite saber el tier.
            // Ver NKR_API.md §4.18 (ballooning IDLE/ACTIVE).
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            // Validar state si vino en el body (opcional, default "active").
            // No-op para "idle" — IDLE es decay automático, no se fuerza
            // desde el panel para no romper el contrato del decay timer.
            if !body.is_empty() {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
                    if let Some(s) = v.get("state").and_then(|x| x.as_str()) {
                        if s == "idle" {
                            return HttpResponse::json(400, serde_json::json!({
                                "error": "explicit_idle_not_supported",
                                "message": "IDLE se aplica automáticamente tras balloon_decay_secs sin renovación. No es seteable desde el panel.",
                            }));
                        }
                        if s != "active" {
                            return HttpResponse::error(400, "invalid_state",
                                Some("state debe ser \"active\" (o body vacío)"));
                        }
                    }
                }
            }
            ipc_to_http(ipc_call(&IpcRequest::BalloonActive {
                nkr_name: (*name).to_string(),
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "actions"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > ACTION_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            // Extract the "action" field — pass string to daemon, which validates.
            let action: String = match serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v.get("action").and_then(|x| x.as_str()).map(|s| s.to_string()))
            {
                Some(a) if ["start", "stop", "restart"].contains(&a.as_str()) => a,
                _ => {
                    return HttpResponse::error(
                        400,
                        "invalid_action",
                        Some("expected {\"action\":\"start|stop|restart\"}"),
                    )
                }
            };
            ipc_to_http(ipc_call(&IpcRequest::Action {
                nkr_name: (*name).to_string(),
                action,
            }))
        }

        ("PATCH", ["api", "v1", "cells", _cell, "instances", name, "config"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let body_str = match std::str::from_utf8(body) {
                Ok(s) => s.to_string(),
                Err(_) => return HttpResponse::error(400, "invalid_utf8", None),
            };
            ipc_to_http(ipc_call(&IpcRequest::PatchConfig {
                nkr_name: (*name).to_string(),
                body_json: body_str,
            }))
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "psql"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let query = match v.get("query").and_then(|x| x.as_str()) {
                Some(q) if !q.is_empty() => q.to_string(),
                _ => return HttpResponse::error(400, "missing_query", None),
            };
            let max_rows = v.get("max_rows")
                .and_then(|x| x.as_u64())
                .unwrap_or(1000)
                .min(10_000) as usize;
            ipc_to_http(ipc_call(&IpcRequest::Psql {
                nkr_name: (*name).to_string(),
                query,
                max_rows,
            }))
        }

        ("GET", ["api", "v1", "cells", _cell, "instances", name, "logs"]) => {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            let tail: Option<usize> = query_get(query, "tail")
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.min(10_000));
            let from_offset: Option<u64> = query_get(query, "from_offset")
                .and_then(|v| v.parse().ok());
            let max_lines: Option<usize> = query_get(query, "max_lines")
                .and_then(|v| v.parse().ok())
                .map(|v: usize| v.clamp(1, 10_000));
            let wait_ms: Option<u64> = query_get(query, "wait_ms")
                .and_then(|v| v.parse().ok())
                .map(|v: u64| v.min(25_000));
            ipc_to_http(ipc_call(&IpcRequest::GetLogs {
                nkr_name: (*name).to_string(),
                tail,
                from_offset,
                max_lines,
                wait_ms,
            }))
        }

        ("GET", ["api", "v1", "cells", cell, "instances", name, "logs", "download"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_logs_download(cell, name)
        }

        ("POST", ["api", "v1", "cells", _cell, "instances", name, "modules", op])
            if matches!(*op, "install" | "upgrade" | "uninstall") =>
        {
            if !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_nkr_name", None);
            }
            if body.len() > 16 * 1024 {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let modules: Vec<String> = match v.get("modules").and_then(|x| x.as_array()) {
                Some(arr) => arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect(),
                None => return HttpResponse::error(400, "missing_modules",
                    Some("expected {\"modules\": [\"...\"], \"admin_login\":..., \"admin_password\":...}")),
            };
            if modules.is_empty() {
                return HttpResponse::error(400, "missing_modules", None);
            }
            let admin_login = match v.get("admin_login").and_then(|x| x.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_login", None),
            };
            let admin_password = match v.get("admin_password").and_then(|x| x.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_password", None),
            };
            ipc_to_http(ipc_call(&IpcRequest::ModulesAction {
                nkr_name: (*name).to_string(),
                op: (*op).to_string(),
                modules,
                admin_login,
                admin_password,
            }))
        }

        // --- Panel-ops endpoints: executed locally by the proxy (no daemon IPC) ---
        // The proxy runs unprivileged but has g:nkr-addons rwx over /mnt/nkr/cells
        // and /mnt/nkr/enterprise via setfacl. git/pip are invoked as subprocesses.

        ("POST", ["api", "v1", "cells", cell, "instances", name, "addons", "git"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_addons_git(cell, name, body)
        }

        ("POST", ["api", "v1", "cells", cell, "enterprise", "git"]) => {
            if !is_safe_identifier(cell) {
                return HttpResponse::error(400, "invalid_cell", None);
            }
            handle_enterprise_git(cell, body)
        }

        ("PUT", ["api", "v1", "cells", cell, "instances", name, "pylibs"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            handle_pylibs_put(cell, name, body)
        }

        ("POST", ["api", "v1", "cells", cell, "instances", name, "dns"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            if body.len() > ACTION_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let dns = match v.get("dns").and_then(|x| x.as_str()) {
                Some(d) if api_http::is_safe_dns(d) => d.to_string(),
                _ => return HttpResponse::error(400, "invalid_dns",
                    Some("expected safe DNS name in body field 'dns'")),
            };
            let enable_ws = v.get("enable_websocket")
                .and_then(|x| x.as_bool())
                .unwrap_or(true);
            ipc_to_http(ipc_call(&IpcRequest::CreateDns {
                nkr_name: (*name).to_string(),
                dns,
                enable_websocket: enable_ws,
            }))
        }

        ("POST", ["api", "v1", "cells", cell, "instances", name, "init-db"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            if body.len() > CREATE_BODY_LIMIT {
                return HttpResponse::error(413, "body_too_large", None);
            }
            let v: serde_json::Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(_) => return HttpResponse::error(400, "invalid_json", None),
            };
            let admin_login = v.get("admin_login").and_then(|x| x.as_str())
                .unwrap_or("admin").to_string();
            let admin_password = match v.get("admin_password").and_then(|x| x.as_str()) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => return HttpResponse::error(400, "missing_admin_password",
                    Some("'admin_password' is required (≥4 chars)")),
            };
            let db_name = v.get("db_name").and_then(|x| x.as_str()).map(|s| s.to_string());
            let demo = v.get("demo").and_then(|x| x.as_bool()).unwrap_or(false);
            let lang = v.get("lang").and_then(|x| x.as_str()).map(|s| s.to_string());
            let country = v.get("country_code").and_then(|x| x.as_str()).map(|s| s.to_string());
            let phone = v.get("phone").and_then(|x| x.as_str()).map(|s| s.to_string());
            ipc_to_http(ipc_call(&IpcRequest::InitDb {
                nkr_name: (*name).to_string(),
                db_name,
                admin_login,
                admin_password,
                demo,
                lang,
                country_code: country,
                phone,
            }))
        }

        ("DELETE", ["api", "v1", "cells", cell, "instances", name, "dns"]) => {
            if !is_safe_identifier(cell) || !is_safe_identifier(name) {
                return HttpResponse::error(400, "invalid_path_segment", None);
            }
            let delete_cert = query_get(query, "delete_cert").as_deref() == Some("1");
            ipc_to_http(ipc_call(&IpcRequest::DeleteDns {
                nkr_name: (*name).to_string(),
                delete_cert,
            }))
        }

        _ => HttpResponse::json(
            404,
            serde_json::json!({"error":"not_found","method":method,"path":path}),
        ),
    }
}

fn handle_create_http(cell_hint: Option<&str>, body: &[u8]) -> HttpResponse {
    if body.len() > CREATE_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let body_str = match std::str::from_utf8(body) {
        Ok(s) => s.to_string(),
        Err(_) => return HttpResponse::error(400, "invalid_utf8", None),
    };

    // Lightweight JSON validation of the fields we care about — daemon
    // re-validates with the typed schema but we sanitize early to give good
    // error messages and prevent obvious injections from reaching the socket.
    let v: serde_json::Value = match serde_json::from_str(&body_str) {
        Ok(v) => v,
        Err(_) => return HttpResponse::error(400, "invalid_json", None),
    };

    let check_ident = |key: &str| -> Option<HttpResponse> {
        v.get(key).and_then(|x| x.as_str()).and_then(|s| {
            if !is_safe_identifier(s) {
                Some(HttpResponse::error(400, &format!("invalid_{}", key), None))
            } else {
                None
            }
        })
    };
    for k in ["nkr_name", "odoo_version", "cell", "source", "pg_version"] {
        if let Some(r) = check_ident(k) {
            return r;
        }
    }
    if let Some(d) = v.get("dns").and_then(|x| x.as_str()) {
        if !is_safe_dns(d) {
            return HttpResponse::error(400, "invalid_dns", None);
        }
    }
    if let Some(ap) = v.get("addons_path").and_then(|x| x.as_str()) {
        if !is_safe_addons_path(ap) {
            return HttpResponse::error(400, "invalid_addons_path", None);
        }
    }
    if let Some(h) = cell_hint {
        if !is_safe_identifier(h) {
            return HttpResponse::error(400, "invalid_cell_in_url", None);
        }
    }

    ipc_to_http(ipc_call(&IpcRequest::CreateInstance {
        cell_hint: cell_hint.map(|s| s.to_string()),
        body_json: body_str,
    }))
}

// =============================================================================
// IPC client wrapper
// =============================================================================

fn ipc_call(req: &IpcRequest) -> Result<IpcResponse, std::io::Error> {
    // Operaciones que pueden tardar mucho necesitan timeout mayor que el default
    // 120s del ipc::call. CreateInstance con admin_user_password incluye
    // compose-up + wait HTTP + JSON-RPC change_password (~50-150s). InitDb,
    // ModulesAction y addons/git también pueden ser largos. 600s es suficiente
    // para todos sin atar al panel a esperas absurdas.
    let timeout = match req {
        IpcRequest::CreateInstance { .. }
        | IpcRequest::InitDb { .. }
        | IpcRequest::ModulesAction { .. }
        | IpcRequest::CreateDns { .. }
        | IpcRequest::Action { .. }
        | IpcRequest::DeleteInstance { .. } => std::time::Duration::from_secs(600),
        _ => std::time::Duration::from_secs(120),
    };
    ipc::call_with_timeout(req, timeout)
}

fn ipc_to_http(res: Result<IpcResponse, std::io::Error>) -> HttpResponse {
    match res {
        Ok(r) => HttpResponse {
            status: r.status,
            body: r.body,
            content_type: r.content_type,
            extra_headers: Vec::new(),
        },
        Err(e) => {
            eprintln!("[NKR-API-SERVER] IPC error: {}", e);
            HttpResponse::error(
                502,
                "daemon_unreachable",
                Some("nkr daemon no responde en UDS"),
            )
        }
    }
}

// =============================================================================
// Panel-ops handlers (git / pylibs)
// =============================================================================
//
// These run inside the proxy, not the root daemon. The proxy speaks to the
// local filesystem via the nkr-addons supplementary group (setfacl) and to
// the network for git/pip clones. Everything the panel sends (repo URL, ref,
// subdir, deploy key, requirements.txt) is validated before touching disk or
// exec, because the proxy sits on the boundary of the tenant's trust domain.

fn handle_addons_git(cell: &str, instance: &str, body: &[u8]) -> HttpResponse {
    if body.len() > GIT_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let req = match parse_git_body(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Target dir: /mnt/nkr/cells/<cell>/instances/<instance>/addons/
    let instance_dir = format!("{}/cells/{}/instances/{}", NKR_ROOT, cell, instance);
    if !std::path::Path::new(&instance_dir).is_dir() {
        return HttpResponse::error(404, "instance_not_found", None);
    }
    let addons_dir = format!("{}/addons", instance_dir);
    let _ = std::fs::create_dir_all(&addons_dir);

    let subdir = req.subdir.as_deref()
        .map(|s| s.to_string())
        .unwrap_or_else(|| derive_repo_basename(&req.repo_url));
    if !is_safe_identifier(&subdir) {
        return HttpResponse::error(400, "invalid_subdir", None);
    }

    // Layout flat: todo módulo termina en addons/<module>/.
    //
    // Flujo Plan B (v1.6.3 — atomic swap, post bug del race con Odoo en vuelo):
    //   1. Staging dir hermano de addons/: `.nkr-addons-new/`.
    //   2. git clone → staging/.nkr-clone-tmp/
    //   3. explode_modules: valida, scanea, computa hashes, mueve módulos
    //      desde .nkr-clone-tmp/ → staging_dir/<m>/ (intra-staging, fast)
    //   4. `renameat2(addons, staging, RENAME_EXCHANGE)` — swap atómico de
    //      directorios en un solo syscall. Cualquier observador (Odoo guest
    //      vía virtio-fs) ve siempre un addons/ coherente: el viejo o el
    //      nuevo, nunca a medias.
    //   5. rm -rf del staging (contiene los viejos módulos, a borrar lazy).
    //
    // Por qué no wipe + populate in-place (flujo legacy v1.6.2):
    //   El wipe destruye archivos que Odoo tiene abiertos. virtio-fs no
    //   resuelve los reads sobre fds dangling y Odoo queda en D-state
    //   (uninterruptible sleep) — ni SIGTERM lo libera. Resultado: zombie
    //   loop, supervisor no puede respawnear, sólo restart full de la VM
    //   recupera. Reproducido 2026-05-10 con tenant intech-devp.
    let staging_dir = format!("{}/.nkr-addons-new", instance_dir);
    let clone_tmp = format!("{}/.nkr-clone-tmp", staging_dir);
    // Cleanup de staging stale (crash de deploy anterior).
    let _ = std::fs::remove_dir_all(&staging_dir);
    if let Err(e) = std::fs::create_dir_all(&staging_dir) {
        return HttpResponse::error(500, "staging_mkdir_failed",
            Some(&format!("crear {}: {}", staging_dir, e)));
    }

    // Forzar clone (ignorar req.action) — clone_tmp está vacío.
    let mut clone_req = req.clone();
    clone_req.action = "clone".to_string();

    let clone_resp = run_git_sync(&clone_req, &clone_tmp);
    if clone_resp.status >= 400 {
        let _ = std::fs::remove_dir_all(&staging_dir);
        return clone_resp;
    }

    let sha = git_head_sha(&clone_tmp).unwrap_or_else(|| "unknown".to_string());

    // Detectar layout, validar, computar hashes y construir staging.
    // El swap atómico ocurre dentro de explode_modules (paso final).
    let explode_result = explode_modules(
        &clone_tmp,
        &addons_dir,
        &staging_dir,
        &subdir,
        &req.repo_url,
        req.reference.as_deref(),
        &sha,
    );

    // Cleanup del staging — tras el swap exitoso contiene los módulos VIEJOS
    // (a borrar). Si explode_modules falló antes del swap, contiene parcial.
    // En ambos casos: borrarlo es seguro.
    let _ = std::fs::remove_dir_all(&staging_dir);

    match explode_result {
        Ok(deploy) => {
            // Auto-reload: SIGUSR1 al PID de la VM → vmm.rs inyecta REL_OD
            // por hvc0 → init guest hace pkill -HUP odoo → master kill workers
            // → respawnean con código fresh. Fire-and-forget: si la VM está
            // apagada o el reload falla, no bloqueamos la response del clone
            // (el panel ya tiene los archivos en disco; el reload es bonus).
            // Ignoramos errores intencionalmente — el panel puede llamar
            // POST /reload manual si quiere certeza, o leer reloaded en la
            // response.
            let mut reloaded = false;
            let mut reload_skipped_reason: Option<&'static str> = None;
            if req.auto_reload {
                match ipc_call(&IpcRequest::ReloadWorkers {
                    nkr_name: instance.to_string(),
                }) {
                    Ok(r) if r.status == 202 => { reloaded = true; }
                    Ok(r) => {
                        eprintln!("[NKR-API-SERVER] auto-reload({}) status={} (clone OK pero workers no recargaron)",
                            instance, r.status);
                        reload_skipped_reason = Some("instance_not_running_or_unknown_pid");
                    }
                    Err(e) => {
                        eprintln!("[NKR-API-SERVER] auto-reload({}) IPC failed: {}", instance, e);
                        reload_skipped_reason = Some("ipc_error");
                    }
                }
            } else {
                reload_skipped_reason = Some("auto_reload=false");
            }
            HttpResponse::json(200, serde_json::json!({
                "repo_url": req.repo_url,
                "ref": req.reference,
                "sha": sha,
                "module_count": deploy.modules.len(),
                "modules": deploy.modules,
                "added": deploy.added,
                "updated": deploy.updated,
                "unchanged": deploy.unchanged,
                "removed": deploy.removed,
                "reloaded": reloaded,
                "reload_skipped_reason": reload_skipped_reason,
            }))
        }
        Err(err_resp) => err_resp,
    }
}

/// Resultado de `explode_modules`: lista plana de módulos publicados +
/// clasificación por diff vs el estado previo del addons/ (basado en
/// tree-hash git per-módulo guardado en `.nkr-source`).
///
/// Bajo Higiene Doble (CLAUDE.md v2.2) addons/ se vacía completamente cada
/// ciclo y se repuebla desde el meta-repo. `removed` lista los módulos que
/// estaban antes y NO vinieron en el ciclo actual — útil para el panel:
/// avisa cuándo un addon dejó de estar en el repo y desapareció del tenant.
struct ExplodeResult {
    modules: Vec<String>,    // back-compat: nombres en orden de descubrimiento
    added: Vec<String>,      // no existían antes en addons/
    updated: Vec<String>,    // existían y el tree-hash cambió
    unchanged: Vec<String>,  // existían y el tree-hash es idéntico
    removed: Vec<String>,    // existían y ya no están en el meta-repo
}

/// Encuentra el directorio git "más cercano" subiendo desde `start` hasta
/// `ceiling` (inclusive). En el caso de submódulos hay múltiples .git en
/// el árbol; cada módulo debe usar el suyo (el del submódulo) o el del
/// padre, según donde viva.
///
/// `.git` puede ser un dir (repo normal) o un archivo (puntero al
/// `.git/modules/<name>` del padre cuando es submódulo). Ambos cuentan.
fn nearest_git_root(
    start: &std::path::Path,
    ceiling: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut cur = start.to_path_buf();
    loop {
        if cur.join(".git").exists() {
            return Some(cur);
        }
        if cur == *ceiling {
            return None;
        }
        match cur.parent() {
            Some(p) if p.starts_with(ceiling) || p == ceiling => cur = p.to_path_buf(),
            _ => return None,
        }
    }
}

/// Llama `git rev-parse HEAD:<rel_path>` para obtener el tree-hash (SHA-1
/// hex de 40 chars) del subdir dentro del repo. Es determinístico — cualquier
/// cambio en el contenido recursivo del subdir produce un hash distinto.
/// Devuelve None si git falla o el path no está en HEAD.
fn git_tree_hash(git_root: &std::path::Path, rel_path: &str) -> Option<String> {
    let arg = if rel_path.is_empty() || rel_path == "." {
        "HEAD^{tree}".to_string()
    } else {
        format!("HEAD:{}", rel_path)
    };
    let out = std::process::Command::new("git")
        .arg("-C").arg(git_root)
        .arg("rev-parse").arg(&arg)
        .output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.len() != 40 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Lee `content_hash=<sha>` de un `.nkr-source` existente. Devuelve None si
/// el archivo no existe, no tiene la línea, o el formato es inválido.
fn read_prev_content_hash(module_dir: &str) -> Option<String> {
    let path = format!("{}/.nkr-source", module_dir);
    let s = std::fs::read_to_string(&path).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("content_hash=") {
            let h = rest.trim();
            if h.len() == 40 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(h.to_string());
            }
        }
    }
    None
}

/// Walks `tmp_dir` recursively looking for any directory whose entry list
/// includes `__manifest__.py`. Skips `.git/` directories and any dotfile
/// directory. Returns a vector of (absolute-path-of-module-dir, dirname)
/// pairs. Dirname is the basename of the module dir, which becomes the
/// publication name in `addons/<dirname>/`.
///
/// Stops descending once it finds `__manifest__.py` in a directory: the
/// subdirs of an Odoo module (data/, models/, wizard/, security/) are not
/// modules themselves, so descending into them would yield false positives
/// if any of them happened to contain a stray `__manifest__.py`.
fn scan_modules_recursive(tmp_dir: &str) -> Result<Vec<(std::path::PathBuf, String)>, String> {
    let mut found: Vec<(std::path::PathBuf, String)> = Vec::new();
    let root = std::path::Path::new(tmp_dir);
    walk_for_modules(root, &mut found)?;
    Ok(found)
}

fn walk_for_modules(
    dir: &std::path::Path,
    acc: &mut Vec<(std::path::PathBuf, String)>,
) -> Result<(), String> {
    // If this dir IS a module, capture and don't descend further.
    if dir.join("__manifest__.py").is_file() {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("invalid dir name at {}", dir.display()))?
            .to_string();
        acc.push((dir.to_path_buf(), name));
        return Ok(());
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir {} failed: {}", dir.display(), e))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        // Skip Git internals and any dotfile dir.
        if name.starts_with('.') {
            continue;
        }
        walk_for_modules(&path, acc)?;
    }
    Ok(())
}

/// Returns (name, [paths]) for any module name that appears more than once
/// across the recursive scan results. Empty vec when no collisions.
fn detect_name_collisions(
    modules: &[(std::path::PathBuf, String)],
) -> Vec<(String, Vec<std::path::PathBuf>)> {
    use std::collections::HashMap;
    let mut by_name: HashMap<String, Vec<std::path::PathBuf>> = HashMap::new();
    for (path, name) in modules {
        by_name.entry(name.clone()).or_default().push(path.clone());
    }
    by_name
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .collect()
}

/// Resultado de la validación estricta de submódulos. Bajo CLAUDE.md v2.2:
/// cada `path = X` declarado en cualquier `.gitmodules` (recursivo) debe
/// ser uno de:
///   (a) un módulo Odoo: `__manifest__.py` en su raíz, o
///   (b) un agrupador: tiene su propio `.gitmodules` (validado recursivamente).
/// Si no es ninguno → `no_manifest` (rechaza basureros de scripts/docs).
/// Si está vacío post-clone → `empty` (clone parcial por PAT scope).
struct SubmoduleValidation {
    /// Submódulo declarado pero el dir está vacío. Causa típica: PAT sin
    /// scope sobre el repo del submódulo, o repo del submódulo borrado.
    empty: Vec<String>,
    /// Submódulo declarado pero ni es módulo (sin `__manifest__.py` en raíz)
    /// ni es agrupador (sin `.gitmodules` propio). Doctrine: el meta-repo
    /// no es un basurero de scripts; cada submódulo debe ser modulo Odoo
    /// o un nivel intermedio de la matrioshka.
    no_manifest: Vec<String>,
}

impl SubmoduleValidation {
    fn is_clean(&self) -> bool {
        self.empty.is_empty() && self.no_manifest.is_empty()
    }
}

/// Validación estricta CLAUDE.md v2.2 (matrioshka): walk recursivo de
/// `.gitmodules`, falla rápido con detalle por submódulo malformado. Se
/// llama ANTES del wipe target — si el árbol está sucio, NO se toca el
/// `addons/` del tenant.
fn validate_submodules_strict(tmp_dir: &str) -> SubmoduleValidation {
    let root = std::path::Path::new(tmp_dir);
    let mut v = SubmoduleValidation {
        empty: Vec::new(),
        no_manifest: Vec::new(),
    };
    walk_gitmodules_strict(root, root, &mut v);
    v
}

fn walk_gitmodules_strict(
    root: &std::path::Path,
    current: &std::path::Path,
    v: &mut SubmoduleValidation,
) {
    let gm = current.join(".gitmodules");
    if !gm.is_file() {
        return;
    }
    let content = match std::fs::read_to_string(&gm) {
        Ok(s) => s,
        Err(_) => return,
    };
    // Minimal INI parser: extract `path = X` lines from any [submodule "..."]
    // section. We don't need to parse the section header — `path =` lines
    // only appear under submodule sections in this format.
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("path") {
            let val = rest
                .trim_start_matches(|c: char| c == ' ' || c == '\t' || c == '=')
                .trim();
            if val.is_empty() {
                continue;
            }
            let sub_path = current.join(val);
            let rel = sub_path
                .strip_prefix(root)
                .unwrap_or(&sub_path)
                .to_string_lossy()
                .into_owned();
            let count = std::fs::read_dir(&sub_path)
                .map(|rd| rd.flatten().count())
                .unwrap_or(0);
            if count == 0 {
                v.empty.push(rel);
                continue;
            }
            let has_manifest = sub_path.join("__manifest__.py").is_file();
            let has_gitmodules = sub_path.join(".gitmodules").is_file();
            if !has_manifest && !has_gitmodules {
                // Doctrine: submódulo declarado pero no es módulo Odoo ni
                // agrupador. Probable basura: docs, scripts, README-only repo.
                v.no_manifest.push(rel);
                continue;
            }
            if has_gitmodules {
                // Agrupador: descender para validar la siguiente capa de
                // la matrioshka. Si además tiene manifest (raro pero posible:
                // un módulo "padre" que orquesta hijos vía submódulos),
                // ambos son válidos — el padre se publica, los hijos también.
                walk_gitmodules_strict(root, &sub_path, v);
            }
        }
    }
}

/// Higiene de Origen (CLAUDE.md v2.2): `git clean -ffdx` sobre la escalera
/// + `git submodule foreach --recursive git clean -ffdx`. Limpia archivos
/// untracked, ignored y modificados en el árbol entero ANTES de validar y
/// publicar. En clone fresco es idempotente; sólo aporta cuando el tmp
/// sobrevive entre llamadas (futuro source ladder persistente) o el clone
/// dejó residuos por hooks/post-clone scripts del operador.
fn git_clean_ladder(tmp_dir: &str) {
    let safe = format!("safe.directory={}", tmp_dir);
    let _ = std::process::Command::new("git")
        .args([
            "-c", &safe,
            "-c", "core.hooksPath=/dev/null",
            "-c", "protocol.allow=user",
            "-C", tmp_dir,
            "clean", "-ffdx",
        ])
        .output();
    let _ = std::process::Command::new("git")
        .args([
            "-c", &safe,
            "-c", "core.hooksPath=/dev/null",
            "-c", "protocol.allow=user",
            "-C", tmp_dir,
            "submodule", "foreach", "--recursive",
            "git clean -ffdx",
        ])
        .output();
}

/// Lee el estado previo de `addons/` para diff: para cada subdir top-level
/// (no dotfiles) lee `<dir>/.nkr-source` y extrae el `content_hash`. El
/// HashMap resultante se compara post-populate para clasificar
/// added/updated/unchanged/removed. Se llama ANTES del wipe.
///
/// Si un módulo existía en addons/ sin `.nkr-source` (creación manual, o
/// flujo legacy pre-tracker), entra al map con hash vacío — al comparar
/// con el nuevo hash queda como `updated` (decisión conservadora: lo
/// reportamos como cambio para que el panel lo note).
fn read_addons_state(addons_dir: &str) -> std::collections::HashMap<String, String> {
    let mut state = std::collections::HashMap::new();
    let entries = match std::fs::read_dir(addons_dir) {
        Ok(e) => e,
        Err(_) => return state,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if name.starts_with('.') {
            // Skip hidden state dirs (.nkr-tmp-*, etc.)
            continue;
        }
        let hash = read_prev_content_hash(&path.to_string_lossy()).unwrap_or_default();
        state.insert(name, hash);
    }
    state
}

/// Swap atómico de dos directorios (Plan B — CLAUDE.md v2.2 + bug fix
/// 2026-05-10 race con Odoo en vuelo).
///
/// Path principal: `renameat2(RENAME_EXCHANGE)` — un solo syscall que
/// intercambia los dos paths atómicamente. Cualquier observador (Odoo guest
/// vía virtio-fs) jamás ve `addons/` inexistente ni a medias. Disponible en
/// Linux ≥ 3.15 sobre ext4/btrfs/xfs/virtiofs (todos los que usa NKR).
///
/// Fallback en ENOSYS (FS exótico, kernel viejo de CI): 2-step rename con
/// rollback en caso de falla intermedia. Hay una ventana <1ms donde el
/// path `a` no existe — aceptable para deploys, no aceptable para tenants
/// con tráfico activo (es exactamente el bug que esto resuelve), pero un
/// kernel sin renameat2 es un escenario tan raro que el fallback es sólo
/// defense-in-depth, no el path normal.
fn atomic_swap_dirs(a: &str, b: &str) -> std::io::Result<()> {
    match renameat2_exchange(a, b) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
            eprintln!("[NKR-ADDONS] renameat2 RENAME_EXCHANGE no soportado por \
                       el FS/kernel ({}); fallback a 2-step rename. Esto NO es \
                       atómico — riesgo de race si hay un guest sirviendo \
                       tráfico durante el deploy.", e);
            atomic_swap_dirs_fallback(a, b)
        }
        Err(e) => Err(e),
    }
}

/// Llamada raw a renameat2 con RENAME_EXCHANGE. Errores POSIX se propagan
/// como `std::io::Error` (ENOSYS = no soportado, EINVAL = paths mal, etc.).
fn renameat2_exchange(a: &str, b: &str) -> std::io::Result<()> {
    let a_c = std::ffi::CString::new(a.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let b_c = std::ffi::CString::new(b.as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            a_c.as_ptr(),
            libc::AT_FDCWD,
            b_c.as_ptr(),
            libc::RENAME_EXCHANGE as libc::c_uint,
        )
    };
    if r != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Fallback 2-step rename con rollback si el segundo paso falla. NO atómico.
fn atomic_swap_dirs_fallback(a: &str, b: &str) -> std::io::Result<()> {
    let parking = format!("{}.nkr-swap-tmp-{}", a, std::process::id());
    // Cleanup previo por las dudas
    let _ = std::fs::remove_dir_all(&parking);
    std::fs::rename(a, &parking)?;
    if let Err(e) = std::fs::rename(b, a) {
        // Rollback best-effort: restaurar a
        let _ = std::fs::rename(&parking, a);
        return Err(e);
    }
    // Final: parking ahora contiene los viejos. El caller lo borra después
    // como parte del cleanup del staging. Pero como acá el "staging" es `b`
    // y `b` ya no existe (lo movimos a `a`), tenemos que renombrar parking
    // a b para que el caller pueda hacer su rm -rf staging_dir uniforme.
    std::fs::rename(&parking, b)?;
    Ok(())
}

/// Detecta si el repo recién clonado es single-module (manifest en raíz),
/// multi-módulo (manifests en subdirs) o tiene submódulos jerárquicos
/// (`.gitmodules` presente, profundidad arbitraria), y mueve los módulos a
/// su posición final bajo `addons/<module>/`.
///
/// Flujo Plan B (v1.6.3 — atomic swap; bug fix race con Odoo en vuelo):
///   1. `git clean -ffdx` recursivo sobre la escalera (defensivo).
///   2. Validación 422 estricta de submódulos.
///   3. Scan de módulos en la escalera (clone_tmp dentro del staging).
///   4. Snapshot del estado previo del `addons/` actual (para diff).
///   5. Compute tree-hashes de cada módulo (en clone_tmp).
///   6. Populate staging: rename cada módulo de `clone_tmp/<rel>` →
///      `staging_dir/<m>/`, escribir `.nkr-source` con tracker.
///   7. Cleanup intra-staging: borrar `clone_tmp/` (sobra el `.git` etc.).
///   8. **Atomic swap**: `renameat2(addons, staging, RENAME_EXCHANGE)`.
///   9. Clasificación added/updated/unchanged/removed contra el snapshot.
///
/// Tras el swap, `addons/` contiene los módulos nuevos y `staging_dir/`
/// contiene los viejos. El caller borra `staging_dir/` después.
///
/// Por qué este flujo evita el race:
///   El swap es 1 syscall atómico — el guest jamás ve `addons/` sin existir
///   ni con estado parcial. Odoo en vuelo termina sus reads sobre fds
///   abiertos del addons viejo (semántica POSIX: los inodes viven hasta que
///   se cierren los fds), pero el próximo `os.listdir()` / `open()` ya cae
///   sobre el addons nuevo. Cero tiempo de bloqueo en D-state.
fn explode_modules(
    clone_tmp: &str,
    addons_dir: &str,
    staging_dir: &str,
    subdir_hint: &str,
    repo_url: &str,
    reference: Option<&str>,
    sha: &str,
) -> Result<ExplodeResult, HttpResponse> {
    let clone_path = std::path::Path::new(clone_tmp);
    let manifest_at_root = clone_path.join("__manifest__.py").exists();
    let has_submodules = clone_path.join(".gitmodules").is_file();

    // ─── STEP 1: Higiene de Origen ─────────────────────────────────────────
    git_clean_ladder(clone_tmp);

    // ─── STEP 2: Validación 422 estricta ───────────────────────────────────
    // Sólo aplica si hay `.gitmodules`. Falla antes de tocar `addons/`.
    if has_submodules {
        let v = validate_submodules_strict(clone_tmp);
        if !v.is_clean() {
            if !v.empty.is_empty() {
                return Err(HttpResponse::json(422, serde_json::json!({
                    "error": "submodule_clone_partial",
                    "message": format!(
                        "{} submódulo(s) no se clonaron — verificar scope del PAT \
                         y que cada repo declarado en .gitmodules sea accesible.",
                        v.empty.len()
                    ),
                    "failed_submodules": v.empty,
                    "remediation": "Confirmar que el PAT tiene Contents:Read sobre \
                                    todos los repos del árbol y reintentar el POST.",
                })));
            }
            return Err(HttpResponse::json(422, serde_json::json!({
                "error": "submodule_no_manifest",
                "message": format!(
                    "{} submódulo(s) declarado(s) en .gitmodules no son módulos \
                     Odoo válidos (no tienen __manifest__.py al raíz ni \
                     .gitmodules anidado).",
                    v.no_manifest.len()
                ),
                "invalid_submodules": v.no_manifest,
                "remediation": "Cada submódulo del meta-repo debe ser un módulo \
                                Odoo (con __manifest__.py al raíz) o un \
                                agrupador con su propio .gitmodules apuntando \
                                a módulos. Eliminar los submódulos inválidos \
                                del .gitmodules y commit + push.",
            })));
        }
    }

    // ─── STEP 3: Scan de módulos ───────────────────────────────────────────
    let mut modules: Vec<(std::path::PathBuf, String)> = Vec::new();

    if has_submodules {
        match scan_modules_recursive(clone_tmp) {
            Ok(found) => modules = found,
            Err(e) => return Err(HttpResponse::error(500, "scan_failed", Some(&e))),
        }
        if modules.is_empty() {
            return Err(HttpResponse::error(422, "no_modules_found",
                Some("el árbol clonado (incluyendo submódulos) no contiene \
                      ningún __manifest__.py")));
        }
        for (path, name) in &modules {
            if !is_safe_identifier(name) {
                return Err(HttpResponse::error(400, "invalid_module_name",
                    Some(&format!("module dir '{}' (en {}) tiene caracteres inválidos",
                        name, path.display()))));
            }
        }
        // Detectar colisión de nombres dentro del mismo deploy.
        let collisions = detect_name_collisions(&modules);
        if !collisions.is_empty() {
            let conflicts_json: Vec<serde_json::Value> = collisions
                .iter()
                .map(|(name, paths)| {
                    let rels: Vec<String> = paths
                        .iter()
                        .map(|p| {
                            p.strip_prefix(clone_path)
                                .unwrap_or(p)
                                .to_string_lossy()
                                .into_owned()
                        })
                        .collect();
                    serde_json::json!({
                        "module_name": name,
                        "found_at": rels,
                    })
                })
                .collect();
            return Err(HttpResponse::json(409, serde_json::json!({
                "error": "module_name_collision",
                "message": "dos o más módulos con el mismo nombre fueron \
                            encontrados en distintas ramas del árbol Git. \
                            Renombrar uno de ellos en el repo y re-mandar.",
                "conflicts": conflicts_json,
                "remediation": "Renombrar el directorio del módulo en uno \
                                de los repos en conflicto y commit + push. \
                                NKR no eligió un ganador automáticamente \
                                para evitar perder código silenciosamente.",
                "repo_url": repo_url,
                "ref": reference,
            })));
        }
    } else if manifest_at_root {
        // Single-module: el módulo ES el clone_tmp entero. Lo movemos como
        // bloque a `staging_dir/<subdir_hint>/`.
        modules.push((clone_path.to_path_buf(), subdir_hint.to_string()));
    } else {
        // Multi-módulo legacy: subdirs directos con __manifest__.py.
        let entries = match std::fs::read_dir(clone_path) {
            Ok(e) => e,
            Err(e) => return Err(HttpResponse::error(500, "scan_failed",
                Some(&e.to_string()))),
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() { continue; }
            let name = match p.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if name.starts_with('.') { continue; } // .git, .github, etc.
            if !p.join("__manifest__.py").exists() { continue; }
            if !is_safe_identifier(&name) {
                return Err(HttpResponse::error(400, "invalid_module_name",
                    Some(&format!("module dir '{}' tiene caracteres inválidos", name))));
            }
            modules.push((p, name));
        }
        if modules.is_empty() {
            return Err(HttpResponse::error(422, "no_modules_found",
                Some("repo no tiene __manifest__.py en raíz ni en subdirs de primer nivel")));
        }
    }

    // ─── STEP 4: Snapshot del estado previo de `addons/` ───────────────────
    let prev_state = read_addons_state(addons_dir);

    // ─── STEP 5: Compute new tree-hashes ───────────────────────────────────
    // Per-módulo via `git rev-parse HEAD:<rel>` contra el `.git` más
    // cercano. Se hace en clone_tmp mientras los src_paths son válidos.
    let mut new_hashes: Vec<Option<String>> = Vec::with_capacity(modules.len());
    for (src_path, _) in &modules {
        let new_hash = if !has_submodules && manifest_at_root {
            git_tree_hash(clone_path, "")
        } else {
            match nearest_git_root(src_path, clone_path) {
                Some(git_root) => {
                    let rel = src_path.strip_prefix(&git_root)
                        .ok()
                        .and_then(|p| p.to_str())
                        .unwrap_or("");
                    git_tree_hash(&git_root, rel)
                }
                None => None,
            }
        };
        new_hashes.push(new_hash);
    }

    // ─── STEP 6: Populate staging (rename intra-staging) ──────────────────
    // Mover cada módulo desde `clone_tmp/<rel>` → `staging_dir/<m>/`. Como
    // clone_tmp es hijo de staging_dir (mismo FS), los renames son atómicos
    // por inode-move. El `addons/` del tenant queda intacto.
    let mut module_names: Vec<String> = Vec::with_capacity(modules.len());

    if !has_submodules && manifest_at_root {
        // Single-module: el clone_tmp completo se renombra a staging/<m>/.
        // Pero clone_tmp es hijo de staging_dir, así que: clone_tmp ya está
        // ahí, sólo le cambiamos el basename de `.nkr-clone-tmp` a `<m>`.
        let m = &modules[0].1;
        let dst = format!("{}/{}", staging_dir, m);
        if let Err(e) = std::fs::rename(clone_tmp, &dst) {
            return Err(HttpResponse::error(500, "move_failed",
                Some(&format!("rename {} → {}: {}", clone_tmp, dst, e))));
        }
        let new_hash = new_hashes[0].clone().unwrap_or_default();
        write_nkr_source(&dst, repo_url, reference, sha, &new_hash);
        module_names.push(m.clone());
    } else {
        for (idx, (src_path, m)) in modules.iter().enumerate() {
            let dst = format!("{}/{}", staging_dir, m);
            if let Err(e) = std::fs::rename(src_path, &dst) {
                return Err(HttpResponse::error(500, "move_failed",
                    Some(&format!("rename {} → {}: {}",
                        src_path.display(), dst, e))));
            }
            let new_hash = new_hashes[idx].clone().unwrap_or_default();
            write_nkr_source(&dst, repo_url, reference, sha, &new_hash);
            module_names.push(m.clone());
        }
    }

    // ─── STEP 7: Cleanup intra-staging ─────────────────────────────────────
    // clone_tmp puede ya no existir (single-module lo renombró). Si queda,
    // contiene .git + archivos sueltos del repo. Borrar.
    let _ = std::fs::remove_dir_all(clone_tmp);

    // ─── STEP 8: Rename per-módulo INTRA-`addons/` (Plan C — fix 2026-05-11) ─
    //
    // **Por qué NO usamos atomic swap del dir top-level** (lo que hacía v1.6.3
    // inicial con renameat2(RENAME_EXCHANGE)):
    //   El swap intercambia el INODE del dir `addons/` entre el viejo y el
    //   nuevo. Pero virtio-fs **NO propaga la invalidación de inode al
    //   guest** — el guest mantiene su dentry/inode cache apuntando al viejo
    //   inode. Cuando el caller hace cleanup del `staging` (= el viejo dir
    //   ahora), virtio-fs en el guest empieza a ver archivos que "existen"
    //   en el listdir pero `open()` retorna ENOENT. Resultado: Odoo carga
    //   la DB del tenant pero los addons del filesystem son fantasmas —
    //   "Some modules are not loaded" + "Missing model queue.job".
    //   Reproducido en intech-devp 2026-05-11 ~02:16 UTC.
    //
    // **Solución**: mantener el inode top-level de `addons/` (= virtio-fs no
    // se confunde), reemplazar cada MÓDULO individualmente con `rename`
    // intra-`addons/`. El guest hace re-stat por entry, no cachea entries
    // viejos cuando el dir raíz no cambia de inode.
    //
    // Algoritmo:
    //   Para cada módulo en staging:
    //     1. Si addons/<m> existe → rename a addons/.nkr-trash-<m>-<unix_ts>
    //        (los fds abiertos por Odoo siguen apuntando al inode viejo
    //        que sigue accesible — semántica POSIX).
    //     2. rename(staging/<m>, addons/<m>) — el nuevo módulo aparece.
    //   Después del REL_OD (Odoo muere → fds cerrados):
    //     3. rm -rf addons/.nkr-trash-* (lazy cleanup en background thread).
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // El trash vive FUERA de `addons/` — Odoo 19 escanea TODOS los dirs
    // de addons_path (incluso dotfiles) y trata de cargar cualquier subdir
    // con __manifest__.py como módulo. Resultado:
    // `FileNotFoundError: Invalid module name: .nkr-trash-...` cada vez
    // que `update_list()` corre (UI Update Apps List, cron de queue_job,
    // button_immediate_upgrade, etc.).
    //
    // Solución: trash en `<instance_dir>/.nkr-trash/` (sibling de addons/,
    // fuera de addons_path). El rename atómico cross-dir funciona en el
    // mismo FS (btrfs del host). Los fds abiertos por Odoo al inode viejo
    // siguen vivos (semántica POSIX) hasta que se cierran post-REL_OD.
    //
    // Reproducido 2026-05-11: el bug previo nombraba el trash con dot
    // INTERIOR (`<m>.nkr-trash-`) lo cual Odoo trataba como módulo con
    // nombre raro y crasheaba `update_list`. El "fix" con dot PREFIX
    // (`.nkr-trash-<ts>-<m>`) tampoco funcionó porque Odoo escanea
    // dotfiles también. La solución definitiva es sacarlo del addons_path.
    // El instance_dir se deriva del staging (que es `<instance>/.nkr-addons-new`).
    // El trash queda en `<instance>/.nkr-trash/` — sibling de `addons/`, fuera
    // del addons_path → Odoo no lo escanea jamás.
    let instance_dir = std::path::Path::new(staging_dir)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| staging_dir.to_string());
    let trash_dir = format!("{}/.nkr-trash", instance_dir);
    let _ = std::fs::create_dir_all(&trash_dir);
    for m in &module_names {
        let dst = format!("{}/{}", addons_dir, m);
        let src = format!("{}/{}", staging_dir, m);
        if std::path::Path::new(&dst).exists() {
            let trash = format!("{}/{}-{}", trash_dir, now_secs, m);
            if let Err(e) = std::fs::rename(&dst, &trash) {
                return Err(HttpResponse::error(500, "module_trash_failed",
                    Some(&format!("rename {} → {}: {}", dst, trash, e))));
            }
        }
        if let Err(e) = std::fs::rename(&src, &dst) {
            return Err(HttpResponse::error(500, "module_install_failed",
                Some(&format!("rename {} → {}: {}", src, dst, e))));
        }
    }
    // Removidos: módulos que estaban en `addons/` y NO vinieron en este ciclo.
    for m in prev_state.keys() {
        if module_names.contains(m) { continue; }
        let old = format!("{}/{}", addons_dir, m);
        if std::path::Path::new(&old).exists() {
            let trash = format!("{}/{}-{}", trash_dir, now_secs, m);
            let _ = std::fs::rename(&old, &trash);
        }
    }
    // Lazy cleanup en background — wait 60s para que Odoo cierre sus fd
    // post-REL_OD antes de borrar físicamente. Barrido oportunista de todo
    // lo que haya en .nkr-trash/ (defense in depth contra huérfanos).
    {
        let trash_dir_owned = trash_dir.clone();
        std::thread::Builder::new()
            .name("nkr-addons-trash-cleanup".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(60));
                if let Ok(entries) = std::fs::read_dir(&trash_dir_owned) {
                    for e in entries.flatten() {
                        let _ = std::fs::remove_dir_all(e.path());
                    }
                }
            })
            .ok();
    }

    // ─── STEP 9: Clasificación contra el snapshot previo ───────────────────
    let mut added: Vec<String> = Vec::new();
    let mut updated: Vec<String> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();

    let new_state: std::collections::HashMap<&str, String> = module_names
        .iter()
        .zip(new_hashes.iter())
        .map(|(m, h)| (m.as_str(), h.clone().unwrap_or_default()))
        .collect();

    for m in &module_names {
        let new = new_state.get(m.as_str()).cloned().unwrap_or_default();
        match prev_state.get(m) {
            None => added.push(m.clone()),
            Some(prev) if !prev.is_empty() && !new.is_empty() && prev == &new => {
                unchanged.push(m.clone());
            }
            Some(_) => updated.push(m.clone()),
        }
    }
    let removed: Vec<String> = prev_state
        .keys()
        .filter(|k| !new_state.contains_key(k.as_str()))
        .cloned()
        .collect();

    Ok(ExplodeResult {
        modules: module_names,
        added,
        updated,
        unchanged,
        removed,
    })
}

fn write_nkr_source(
    module_dir: &str,
    repo_url: &str,
    reference: Option<&str>,
    sha: &str,
    content_hash: &str,
) {
    let content = format!(
        "repo_url={}\nref={}\nsha={}\ncontent_hash={}\n",
        repo_url,
        reference.unwrap_or(""),
        sha,
        content_hash,
    );
    let _ = std::fs::write(format!("{}/.nkr-source", module_dir), content);
}

fn handle_enterprise_git(cell: &str, body: &[u8]) -> HttpResponse {
    if body.len() > GIT_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let req = match parse_git_body(body) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // Enterprise lives at /mnt/nkr/enterprise/<odoo_version>/ — version
    // is read from the cell's cell.yml rather than passed by the panel, so
    // mismatches are impossible.
    let version = match read_cell_odoo_version(cell) {
        Some(v) => v,
        None => return HttpResponse::error(404, "cell_not_found", None),
    };
    if !is_safe_identifier(&version) {
        return HttpResponse::error(500, "invalid_cell_version", None);
    }
    let ent_dir = format!("{}/enterprise/{}", NKR_ROOT, version);
    let _ = std::fs::create_dir_all(&ent_dir);
    run_git_sync(&req, &ent_dir)
}

fn handle_pylibs_put(cell: &str, instance: &str, body: &[u8]) -> HttpResponse {
    if body.len() > PYLIBS_BODY_LIMIT {
        return HttpResponse::error(413, "body_too_large", None);
    }
    let v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return HttpResponse::error(400, "invalid_json", None),
    };
    let reqs_txt = match v.get("requirements_txt").and_then(|x| x.as_str()) {
        Some(s) if !s.is_empty() && s.len() <= 64 * 1024 => s.to_string(),
        _ => return HttpResponse::error(400, "missing_or_invalid_requirements_txt", None),
    };

    let instance_dir = format!("{}/cells/{}/instances/{}", NKR_ROOT, cell, instance);
    if !std::path::Path::new(&instance_dir).is_dir() {
        return HttpResponse::error(404, "instance_not_found", None);
    }
    let pylibs_dir = format!("{}/pylibs", instance_dir);
    let lib_dir = format!("{}/lib", pylibs_dir);
    let _ = std::fs::create_dir_all(&lib_dir);
    let reqs_path = format!("{}/requirements.txt", pylibs_dir);
    if let Err(e) = std::fs::write(&reqs_path, &reqs_txt) {
        return HttpResponse::error(500, "write_requirements_failed",
            Some(&e.to_string()));
    }

    // pip install --target=<lib> --prefer-binary --no-cache-dir --upgrade
    // --prefer-binary: use wheels when available, fall back to sdist only if
    // pure-Python (no compilation needed — we don't ship gcc in the proxy).
    let mut cmd = std::process::Command::new("pip3");
    cmd.args([
        "install",
        "-r", &reqs_path,
        "--target", &lib_dir,
        "--prefer-binary",
        "--no-cache-dir",
        "--no-compile",
        "--upgrade",
        "--disable-pip-version-check",
    ]);
    // The systemd unit sets UMask=0077 → pip would write files 0600 / dirs 0700.
    // The guest Odoo runs as uid 101 and virtio-fs does no UID remapping, so
    // those imports would fail with PermissionError. Set umask 022 in the child
    // (after fork, before exec — single-threaded, safe) so pip writes 0644/0755
    // directly. No post-hoc `chmod -R` (which used to log "Operation not
    // permitted" against the root-owned `pylibs/lib/` created at clone time).
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| { libc::umask(0o022); Ok(()) });
    }
    let (status, stdout, stderr) = match run_with_timeout(cmd, PIP_TIMEOUT_S) {
        Ok(t) => t,
        Err(e) => return HttpResponse::error(500, "pip_spawn_failed", Some(&e)),
    };
    let tail = tail_lines(&format!("{}\n{}", stdout, stderr), 50);
    if !status {
        return HttpResponse::json(422, serde_json::json!({
            "error": "pip_install_failed",
            "log_tail": tail,
        }));
    }
    HttpResponse::json(200, serde_json::json!({
        "installed": true,
        "lib_dir": lib_dir,
        "pip_log_tail": tail,
    }))
}

// ----- Helpers ---------------------------------------------------------------

#[derive(Clone)]
struct GitReq {
    repo_url: String,
    subdir: Option<String>,
    reference: Option<String>,  // git ref (branch/tag/sha); default: remote HEAD
    action: String,              // "sync" | "clone" | "pull"
    deploy_key_b64: Option<String>,
    github_token: Option<String>, // PAT HTTPS alternative to deploy_key
    auto_reload: bool,           // post-clone: SIGUSR1 → REL_OD → pkill -HUP odoo (default true)
}

fn parse_git_body(body: &[u8]) -> Result<GitReq, HttpResponse> {
    let v: serde_json::Value = serde_json::from_slice(body)
        .map_err(|_| HttpResponse::error(400, "invalid_json", None))?;
    let repo_url = v.get("repo_url").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if !is_safe_git_url(&repo_url) {
        return Err(HttpResponse::error(400, "invalid_repo_url",
            Some("only git@github.com:*, https://github.com/*, gitlab.com are accepted")));
    }
    let subdir = v.get("subdir").and_then(|x| x.as_str()).map(|s| s.to_string());
    let reference = v.get("ref").and_then(|x| x.as_str()).map(|s| s.to_string());
    if let Some(ref r) = reference {
        if !is_safe_git_ref(r) {
            return Err(HttpResponse::error(400, "invalid_ref", None));
        }
    }
    let action = v.get("action").and_then(|x| x.as_str()).unwrap_or("sync").to_string();
    if !matches!(action.as_str(), "sync" | "clone" | "pull") {
        return Err(HttpResponse::error(400, "invalid_action",
            Some("expected sync|clone|pull")));
    }
    let deploy_key_b64 = v.get("deploy_key_b64")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let github_token = v.get("github_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    // Validación básica del PAT: GitHub tokens son [A-Za-z0-9_]{≤100}.
    if let Some(ref t) = github_token {
        if t.is_empty() || t.len() > 256
            || !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(HttpResponse::error(400, "invalid_github_token", None));
        }
        // PAT sólo tiene sentido con HTTPS URLs.
        if !repo_url.starts_with("https://") {
            return Err(HttpResponse::error(400, "github_token_requires_https_url",
                Some("PAT auth only works with https://github.com/... URLs; use deploy_key_b64 for SSH (git@...)")));
        }
    }
    // auto_reload: tras clone OK, dispara reload de workers Odoo via SIGUSR1
    // (SIN reiniciar la VM). Default true — Odoo necesita ver el código nuevo
    // y inotify NO funciona vía virtio-fs. Si el panel quiere control manual
    // (no reload), pasar false explícito.
    let auto_reload = v.get("auto_reload")
        .and_then(|x| x.as_bool())
        .unwrap_or(true);
    Ok(GitReq { repo_url, subdir, reference, action, deploy_key_b64, github_token, auto_reload })
}

fn run_git_sync(req: &GitReq, target: &str) -> HttpResponse {
    let target_path = std::path::Path::new(target);
    let has_git = target_path.join(".git").is_dir();

    // Materialize deploy key if present. tmp_key_path stays in scope until the
    // end of this function so the file isn't dropped while git is running.
    let tmp_key_path = match &req.deploy_key_b64 {
        Some(b64) => match write_deploy_key(b64) {
            Ok(p) => Some(p),
            Err(e) => return HttpResponse::error(400, "invalid_deploy_key", Some(&e)),
        },
        None => None,
    };

    // Si hay github_token, inyectar como x-access-token en la URL HTTPS. Sólo
    // en memoria, no se persiste ni se loguea.
    let effective_url: String = match &req.github_token {
        Some(tok) if req.repo_url.starts_with("https://") => {
            let after = req.repo_url.trim_start_matches("https://");
            format!("https://x-access-token:{}@{}", tok, after)
        }
        _ => req.repo_url.clone(),
    };

    let (action_executed, clone_res, pull_res) = match (req.action.as_str(), has_git) {
        ("clone", true) => (
            "clone_skipped_exists",
            None::<(bool, String, String)>,
            None,
        ),
        ("clone", false) | ("sync", false) => {
            // --branch handles both branches and tags at clone time.
            // For commit SHAs, caller must clone first then pull with a ref.
            // The PAT is also forwarded so git_clone can install
            // url.insteadOf rewrites that cover recursive submodule clones
            // under the same owner — the parent already authenticates via
            // effective_url, but submodule URLs read from .gitmodules
            // (typically SSH or vanilla HTTPS) need the rewrite to inherit
            // the credential.
            let r = git_clone(
                &effective_url,
                target,
                req.reference.as_deref(),
                tmp_key_path.as_deref(),
                req.github_token.as_deref(),
            );
            ("clone", Some(r), None)
        }
        ("pull", true) | ("sync", true) => {
            // Para pull con PAT necesitamos re-setear el remote URL, sino
            // el pull usa el URL guardado en .git/config (que podría venir
            // de un clone hecho sin token, ej. público → privado).
            if req.github_token.is_some() {
                let _ = std::process::Command::new("git")
                    .args(["-c", &format!("safe.directory={}", target),
                           "-c", "core.hooksPath=/dev/null",
                           "-c", "protocol.allow=user",
                           "-C", target, "remote", "set-url",
                           "origin", &effective_url])
                    .output();
            }
            let r = git_pull(target, req.reference.as_deref(), tmp_key_path.as_deref());
            // Restaurar el URL SIN token después del pull para no dejar
            // credenciales en .git/config.
            if req.github_token.is_some() {
                let _ = std::process::Command::new("git")
                    .args(["-c", &format!("safe.directory={}", target),
                           "-c", "core.hooksPath=/dev/null",
                           "-c", "protocol.allow=user",
                           "-C", target, "remote", "set-url",
                           "origin", &req.repo_url])
                    .output();
            }
            ("pull", None, Some(r))
        }
        ("pull", false) => {
            return HttpResponse::error(409, "not_a_repo",
                Some("target dir has no .git — call action=clone first"));
        }
        _ => unreachable!("action validated earlier"),
    };

    // Después de clone exitoso con token: re-escribir el URL en .git/config
    // SIN el token, para no persistir credenciales.
    if req.github_token.is_some() && matches!(action_executed, "clone")
        && matches!(&clone_res, Some((true, _, _))) {
        let _ = std::process::Command::new("git")
            .args(["-c", &format!("safe.directory={}", target),
                   "-c", "core.hooksPath=/dev/null",
                   "-c", "protocol.allow=user",
                   "-C", target, "remote", "set-url",
                   "origin", &req.repo_url])
            .output();
    }

    // Ref already handled inside clone (--branch) or pull (FETCH_HEAD), so no
    // second checkout step here.
    let checkout_res: Option<(bool, String, String)> = None;

    // Best-effort scrub of the deploy key before returning.
    if let Some(ref p) = tmp_key_path {
        let _ = std::fs::remove_file(p);
    }

    // Collate errors.
    for (label, res) in [
        ("clone", &clone_res),
        ("pull", &pull_res),
        ("checkout", &checkout_res),
    ] {
        if let Some((false, out, err)) = res {
            // Scrub embedded credentials from git's stdout/stderr before
            // they flow into journald or the panel's log_tail. Patterns:
            //   https://x-access-token:<PAT>@github.com/...
            //   https://oauth2:<PAT>@gitlab.com/...
            //   https://user:password@host/...
            // The classifier still works because the substituted text
            // ("***") doesn't match any of the auth-error tokens.
            let tail_raw = tail_lines(&format!("{}\n{}", out, err), 30);
            let tail = scrub_url_credentials(&tail_raw);
            let full = format!("{}\n{}", out, err);
            let lower = full.to_lowercase();
            let safe_repo_url = scrub_url_credentials(&req.repo_url);

            // Clasificación específica: distinguir auth (panel fix → agregar
            // token/deploy_key) de otras fallas (network, bad ref, etc.).
            let (status_code, error_slug, hint): (u16, String, &'static str) =
                if lower.contains("could not read username")
                    || lower.contains("authentication failed")
                    || lower.contains("invalid username or password")
                {
                    (401, "git_auth_required".into(),
                     "repo privado HTTPS sin credenciales. Reintentar con 'github_token' en el body (PAT) o usando SSH con 'deploy_key_b64' + repo_url git@github.com:...")
                } else if lower.contains("permission denied (publickey)")
                       || lower.contains("could not read from remote repository")
                {
                    (401, "git_ssh_auth_failed".into(),
                     "La SSH deploy key no tiene acceso al repo. Verificar que esté agregada en Settings → Deploy keys del repo en GitHub/GitLab.")
                } else if lower.contains("repository not found")
                       || lower.contains("not found")
                       || lower.contains("404")
                {
                    (404, "git_repo_not_found".into(),
                     "Repo no existe o el token no tiene permiso sobre él. Verificar repo_url y scope del token.")
                } else if lower.contains("remote branch")
                       && lower.contains("not found")
                {
                    (400, "git_ref_not_found".into(),
                     "La rama/tag pasada en 'ref' no existe en el remote.")
                } else if lower.contains("timeout") || lower.contains("timed out") {
                    (504, "git_timeout".into(),
                     "Git tardó más de 180s. Posible red lenta o repo muy grande.")
                } else {
                    (422, format!("git_{}_failed", label),
                     "git devolvió error no clasificado. Revisar log_tail.")
                };

            eprintln!("[NKR-API-SERVER] {} target={} repo={} ref={:?} code={}",
                error_slug, target, safe_repo_url, req.reference, status_code);
            eprintln!("[NKR-API-SERVER] git stdout/stderr tail:\n{}", tail);
            return HttpResponse::json(status_code, serde_json::json!({
                "error": error_slug,
                "message": hint,
                "repo_url": safe_repo_url,
                "ref": req.reference,
                "target": target,
                "log_tail": tail,
            }));
        }
    }

    // Fetch SHA and path for response.
    let sha = git_head_sha(target).unwrap_or_else(|| "unknown".to_string());
    HttpResponse::json(200, serde_json::json!({
        "action": action_executed,
        "path": target,
        "sha": sha,
        "ref": req.reference,
    }))
}

fn git_clone(
    url: &str,
    target: &str,
    reference: Option<&str>,
    key_path: Option<&str>,
    github_token: Option<&str>,
) -> (bool, String, String) {
    let mut cmd = std::process::Command::new("git");

    // Holders for the lifetime of the args slice. They live on the stack of
    // this function so the &str slices below stay valid until cmd.args runs.
    let pat_ssh;
    let pat_https;

    // Disable hooks and non-user protocols — a malicious repo could bring a
    // post-checkout/post-merge hook (via templatedir, etc.) that would run as
    // `nkr-api` during the clone. These flags propagate to recursive sub-git
    // invocations via GIT_CONFIG_PARAMETERS env var.
    let mut args: Vec<&str> = vec![
        "-c", "core.hooksPath=/dev/null",
        "-c", "protocol.allow=user",
    ];

    // If a PAT was supplied, rewrite SSH and bare-HTTPS URLs to embed the
    // token. This makes private submodules (and submodules-of-submodules)
    // under the same owner clone transparently with a single credential —
    // the .gitmodules in each repo can keep declaring SSH or vanilla HTTPS
    // URLs. The url.insteadOf rules also propagate to recursive sub-gits.
    if let Some(pat) = github_token {
        pat_ssh = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=git@github.com:",
            pat
        );
        pat_https = format!(
            "url.https://x-access-token:{}@github.com/.insteadOf=https://github.com/",
            pat
        );
        args.push("-c");
        args.push(&pat_ssh);
        args.push("-c");
        args.push(&pat_https);
    }

    args.extend_from_slice(&[
        "clone",
        "--depth", "1",
        "--recurse-submodules",   // resolve submodules recursively (any depth)
        "--shallow-submodules",   // --depth=1 also for each submodule
        "--jobs", "2",            // limited parallelism (see AUDIT_GH §5.7 on rate limits)
    ]);
    if let Some(r) = reference {
        args.push("--branch");
        args.push(r);
    }
    args.push("--");
    args.push(url);
    args.push(target);
    cmd.args(&args);
    apply_git_ssh(&mut cmd, key_path);
    match run_with_timeout(cmd, GIT_TIMEOUT_S) {
        Ok(t) => t,
        Err(e) => (false, String::new(), e),
    }
}

fn git_pull(target: &str, reference: Option<&str>, key_path: Option<&str>) -> (bool, String, String) {
    // With a ref: fetch it explicitly (covers tags/branches not tracked locally),
    // then hard-reset onto FETCH_HEAD so we're always at the panel-requested
    // state regardless of the local history.
    // Without a ref: plain ff-only pull against the current branch.
    let safe_dir = format!("safe.directory={}", target);
    if let Some(r) = reference {
        let mut fetch = std::process::Command::new("git");
        fetch.args(["-c", &safe_dir,
                    "-c", "core.hooksPath=/dev/null",
                    "-c", "protocol.allow=user",
                    "-C", target, "fetch", "--depth", "1", "origin", "--", r]);
        apply_git_ssh(&mut fetch, key_path);
        match run_with_timeout(fetch, GIT_TIMEOUT_S) {
            Ok((false, o, e)) => return (false, o, e),
            Err(e) => return (false, String::new(), e),
            _ => {}
        }
        let mut reset = std::process::Command::new("git");
        reset.args(["-c", &safe_dir,
                    "-c", "core.hooksPath=/dev/null",
                    "-C", target, "reset", "--hard", "FETCH_HEAD"]);
        match run_with_timeout(reset, GIT_TIMEOUT_S) {
            Ok(t) => t,
            Err(e) => (false, String::new(), e),
        }
    } else {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["-c", &safe_dir,
                  "-c", "core.hooksPath=/dev/null",
                  "-c", "protocol.allow=user",
                  "-C", target, "pull", "--ff-only"]);
        apply_git_ssh(&mut cmd, key_path);
        match run_with_timeout(cmd, GIT_TIMEOUT_S) {
            Ok(t) => t,
            Err(e) => (false, String::new(), e),
        }
    }
}

fn git_head_sha(target: &str) -> Option<String> {
    // `safe.directory=<target>` (NOT `*`) — a global wildcard would disable
    // CVE-2022-24765 process-wide, allowing RCE if an attacker can plant a
    // .git with hooks under any readable dir. Scope it to the dir the panel
    // requested, and disable hooks for safety.
    let out = std::process::Command::new("git")
        .args(["-c", &format!("safe.directory={}", target),
               "-c", "core.hooksPath=/dev/null",
               "-c", "protocol.allow=user",
               "-C", target, "rev-parse", "HEAD"])
        .output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        None
    }
}

fn apply_git_ssh(cmd: &mut std::process::Command, key_path: Option<&str>) {
    if let Some(k) = key_path {
        // accept-new: trust unknown hosts on first contact. Alternative is
        // a pre-populated /etc/ssh/ssh_known_hosts with GitHub/GitLab keys.
        let ssh = format!("ssh -i {} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null -o BatchMode=yes", k);
        cmd.env("GIT_SSH_COMMAND", ssh);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
}

fn write_deploy_key(b64: &str) -> Result<String, String> {
    if b64.len() > 16 * 1024 {
        return Err("deploy_key too large".into());
    }
    let decoded = b64_decode(b64).map_err(|e| format!("base64: {}", e))?;
    if !decoded.starts_with(b"-----BEGIN") {
        return Err("not a PEM private key".into());
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos()).unwrap_or(0);
    let path = format!("/tmp/nkr-api-dkey-{}-{}", pid, nanos);
    // O_CREAT | O_EXCL | O_WRONLY, mode 0600
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true).create_new(true).mode(0o600)
        .open(&path).map_err(|e| e.to_string())?;
    use std::io::Write as _;
    f.write_all(&decoded).map_err(|e| e.to_string())?;
    // Deploy keys often need a trailing newline.
    if !decoded.ends_with(b"\n") {
        f.write_all(b"\n").map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    // Minimal RFC 4648 base64 decoder (skip whitespace, no alphabet deps).
    const T: [i8; 256] = {
        let mut t = [-1i8; 256];
        let mut i = 0u8;
        while i < 26 { t[(b'A' + i) as usize] = i as i8; i += 1; }
        i = 0;
        while i < 26 { t[(b'a' + i) as usize] = (26 + i) as i8; i += 1; }
        i = 0;
        while i < 10 { t[(b'0' + i) as usize] = (52 + i) as i8; i += 1; }
        t[b'+' as usize] = 62; t[b'/' as usize] = 63;
        t[b'-' as usize] = 62; t[b'_' as usize] = 63; // url-safe
        t
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes() {
        if b == b'=' || b == b'\n' || b == b'\r' || b == b' ' || b == b'\t' {
            continue;
        }
        let v = T[b as usize];
        if v < 0 { return Err(format!("invalid char {:?}", b as char)); }
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn run_with_timeout(
    mut cmd: std::process::Command,
    timeout_s: u64,
) -> Result<(bool, String, String), String> {
    use std::io::Read;
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_s);

    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => {
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() { let _ = s.read_to_string(&mut out); }
                let mut err = String::new();
                if let Some(mut s) = child.stderr.take() { let _ = s.read_to_string(&mut err); }
                return Ok((status.success(), out, err));
            }
            None => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timeout after {}s", timeout_s));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Replaces `<scheme>://<user>:<password>@<host>/...` segments with
/// `<scheme>://***:***@<host>/...`. Used to strip PATs / basic-auth tokens
/// embedded in URLs before they reach journald or the panel's `log_tail`.
/// Hand-rolled to avoid pulling in the `regex` crate just for this.
/// Operates byte-by-byte on the search markers (':', '@', '/', whitespace —
/// all single-byte ASCII), so multi-byte UTF-8 in surrounding text is
/// preserved untouched.
fn scrub_url_credentials(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < input.len() {
        // Look for "://" — the start of a possible URL.
        if i + 3 <= input.len() && &bytes[i..i + 3] == b"://" {
            out.push_str("://");
            let scan_start = i + 3;
            // Walk forward looking for ':' and '@' in the authority section.
            // Stop at '/', whitespace, or end-of-string.
            let mut j = scan_start;
            let mut colon_at: Option<usize> = None;
            let mut at_at: Option<usize> = None;
            while j < input.len() {
                let b = bytes[j];
                if b == b'/' || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                    break;
                }
                if b == b':' && colon_at.is_none() {
                    colon_at = Some(j);
                }
                if b == b'@' {
                    at_at = Some(j);
                    break;
                }
                j += 1;
            }
            if let (Some(_c), Some(at)) = (colon_at, at_at) {
                out.push_str("***:***@");
                i = at + 1;
                continue;
            }
            // No credentials → copy the authority slice unmodified (it's
            // valid UTF-8 because the surrounding string is).
            out.push_str(&input[scan_start..j]);
            i = j;
            continue;
        }
        // Default: copy one full UTF-8 char.
        let ch_start = i;
        // Find the end of this UTF-8 char.
        let mut ch_end = ch_start + 1;
        while ch_end < input.len() && (bytes[ch_end] & 0xC0) == 0x80 {
            ch_end += 1;
        }
        out.push_str(&input[ch_start..ch_end]);
        i = ch_end;
    }
    out
}

fn derive_repo_basename(url: &str) -> String {
    // git@host:owner/repo.git or https://host/owner/repo.git → "repo"
    let last = url.rsplit(&['/', ':'][..]).next().unwrap_or(url);
    last.strip_suffix(".git").unwrap_or(last).to_string()
}

fn read_cell_odoo_version(cell: &str) -> Option<String> {
    let path = format!("{}/cells/{}/cell.yml", NKR_ROOT, cell);
    let content = std::fs::read_to_string(&path).ok()?;
    // Minimal YAML scan: `odoo_version: '17.0'` or `odoo_version: 17.0`.
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("odoo_version:") {
            let v = rest.trim().trim_matches(&['\'', '"'][..]).to_string();
            if !v.is_empty() { return Some(v); }
        }
    }
    None
}

/// Descarga `odoo.log` del tenant como text/plain. Cap defensivo: 64 MiB en RAM.
/// Si el archivo excede el cap, devuelve los últimos 64 MiB con un header
/// `X-NKR-Log-Truncated: <original_size>`. El panel puede entonces caer a
/// `GET /logs?from_offset=N` para paginar el resto.
const LOGS_DOWNLOAD_CAP: u64 = 64 * 1024 * 1024;

fn handle_logs_download(cell: &str, instance: &str) -> HttpResponse {
    use std::io::{Read, Seek, SeekFrom};

    let log_path = format!(
        "{}/cells/{}/instances/{}/logs/odoo.log",
        NKR_ROOT, cell, instance
    );
    let mut f = match std::fs::File::open(&log_path) {
        Ok(f) => f,
        Err(_) => return HttpResponse::error(404, "log_not_found", None),
    };
    let file_size = match f.seek(SeekFrom::End(0)) {
        Ok(n) => n,
        Err(_) => return HttpResponse::error(500, "seek_failed", None),
    };
    let (start, truncated) = if file_size > LOGS_DOWNLOAD_CAP {
        (file_size - LOGS_DOWNLOAD_CAP, true)
    } else {
        (0u64, false)
    };
    if f.seek(SeekFrom::Start(start)).is_err() {
        return HttpResponse::error(500, "seek_failed", None);
    }
    let mut buf = Vec::with_capacity((file_size - start) as usize);
    if f.take(LOGS_DOWNLOAD_CAP).read_to_end(&mut buf).is_err() {
        return HttpResponse::error(500, "read_failed", None);
    }
    let body = String::from_utf8_lossy(&buf).into_owned();

    let mut resp = HttpResponse {
        status: 200,
        body,
        content_type: "text/plain; charset=utf-8".to_string(),
        extra_headers: Vec::new(),
    };
    resp.extra_headers.push((
        "Content-Disposition".into(),
        format!("attachment; filename=\"{}-odoo.log\"", instance),
    ));
    if truncated {
        resp.extra_headers.push((
            "X-NKR-Log-Truncated".into(),
            file_size.to_string(),
        ));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_url_credentials_strips_pat() {
        let input = "remote: https://x-access-token:ghp_abc123XYZ@github.com/org/repo.git failed";
        let out = scrub_url_credentials(input);
        assert!(!out.contains("ghp_abc123XYZ"), "PAT must not survive scrub: {}", out);
        assert!(out.contains("***:***@github.com/org/repo.git"));
    }

    #[test]
    fn scrub_url_credentials_strips_basic_auth() {
        let input = "fatal: cannot fetch from https://user:secret-pw@gitlab.example.com/x/y.git";
        let out = scrub_url_credentials(input);
        assert!(!out.contains("secret-pw"));
        assert!(out.contains("***:***@gitlab.example.com"));
    }

    #[test]
    fn scrub_url_credentials_leaves_clean_urls_alone() {
        let input = "remote: https://github.com/org/repo.git pulling main";
        assert_eq!(scrub_url_credentials(input), input);
    }

    #[test]
    fn scrub_url_credentials_preserves_unicode() {
        // Multi-byte UTF-8 in surrounding text must survive.
        let input = "ramo: éxito — https://u:p@host/r café";
        let out = scrub_url_credentials(input);
        assert!(out.contains("éxito"));
        assert!(out.contains("café"));
        assert!(out.contains("***:***@host/r"));
    }

    // ---- Submodule tree handling ----------------------------------------

    use std::fs;
    use std::path::PathBuf;

    fn tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("nkr-gh-{}-{}", label, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_manifest(dir: &std::path::Path) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("__manifest__.py"), b"{}").unwrap();
    }

    #[test]
    fn scan_finds_modules_at_arbitrary_depth() {
        let tmp = tempdir("scan-deep");
        // Layout:
        //   tmp/module-direct/__manifest__.py             (depth 1)
        //   tmp/group-frontend/module-x/__manifest__.py   (depth 2)
        //   tmp/group-backend/sub/module-z/__manifest__.py(depth 3)
        write_manifest(&tmp.join("module-direct"));
        write_manifest(&tmp.join("group-frontend/module-x"));
        write_manifest(&tmp.join("group-backend/sub/module-z"));

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        let names: std::collections::HashSet<String> =
            found.iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(names.len(), 3, "found = {:?}", found);
        assert!(names.contains("module-direct"));
        assert!(names.contains("module-x"));
        assert!(names.contains("module-z"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_skips_dotfile_dirs() {
        let tmp = tempdir("scan-skip-dot");
        // .git/ should NEVER be descended into — even if it contained a
        // bogus __manifest__.py we must not pick it up.
        fs::create_dir_all(tmp.join(".git/modules/foo")).unwrap();
        fs::write(tmp.join(".git/modules/foo/__manifest__.py"), b"{}").unwrap();
        write_manifest(&tmp.join("good-module"));

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        let names: Vec<String> = found.iter().map(|(_, n)| n.clone()).collect();
        assert_eq!(names, vec!["good-module".to_string()]);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn scan_does_not_descend_into_a_module() {
        // A module dir contains data/, models/, security/, wizard/ subdirs.
        // Those are NOT modules — they are subdirs of one module. Scan must
        // stop at the first __manifest__.py found in the chain.
        let tmp = tempdir("scan-no-descend");
        write_manifest(&tmp.join("module-a"));
        fs::create_dir_all(tmp.join("module-a/models")).unwrap();
        fs::write(tmp.join("module-a/models/__init__.py"), b"").unwrap();
        // Stray manifest deep inside (paranoid case): ignored because the
        // walk stops at module-a's manifest.
        fs::write(tmp.join("module-a/models/__manifest__.py"), b"{}").unwrap();

        let found = scan_modules_recursive(&tmp.to_string_lossy()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "module-a");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn detect_collisions_flags_duplicates() {
        let mods: Vec<(PathBuf, String)> = vec![
            (PathBuf::from("/tmp/a/module-x"), "module-x".to_string()),
            (PathBuf::from("/tmp/b/module-x"), "module-x".to_string()),
            (PathBuf::from("/tmp/a/module-y"), "module-y".to_string()),
        ];
        let cols = detect_name_collisions(&mods);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].0, "module-x");
        assert_eq!(cols[0].1.len(), 2);
    }

    #[test]
    fn detect_collisions_returns_empty_when_unique() {
        let mods: Vec<(PathBuf, String)> = vec![
            (PathBuf::from("/tmp/a/m1"), "m1".to_string()),
            (PathBuf::from("/tmp/b/m2"), "m2".to_string()),
        ];
        let cols = detect_name_collisions(&mods);
        assert!(cols.is_empty());
    }

    #[test]
    fn validate_submodules_strict_detects_empty_dir() {
        let tmp = tempdir("val-empty");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "module-a"]
    path = module-a
    url = https://github.com/acme/module-a.git
[submodule "module-b"]
    path = module-b
    url = https://github.com/acme/module-b.git
"#).unwrap();
        write_manifest(&tmp.join("module-a"));
        // module-b dir created but left empty (clone of that submodule failed)
        fs::create_dir_all(tmp.join("module-b")).unwrap();

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(!v.is_clean());
        assert!(v.empty.iter().any(|p| p.contains("module-b")),
            "got empty list: {:?}", v.empty);
        assert!(v.no_manifest.is_empty());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_strict_recurses_into_nested_gitmodules() {
        let tmp = tempdir("val-nested");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "group-frontend"]
    path = group-frontend
    url = https://github.com/acme/group-frontend.git
"#).unwrap();
        fs::create_dir_all(tmp.join("group-frontend")).unwrap();
        fs::write(tmp.join("group-frontend/.gitmodules"), r#"
[submodule "nested-module"]
    path = nested-module
    url = https://github.com/acme/nested.git
"#).unwrap();
        // nested-module dir created but empty (auth failed for the nested repo)
        fs::create_dir_all(tmp.join("group-frontend/nested-module")).unwrap();

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(!v.is_clean());
        assert!(v.empty.iter().any(|p| p.contains("nested-module")),
            "got empty list: {:?}", v.empty);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_strict_passes_when_all_populated() {
        let tmp = tempdir("val-ok");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "module-a"]
    path = module-a
    url = https://github.com/acme/module-a.git
"#).unwrap();
        write_manifest(&tmp.join("module-a"));

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(v.is_clean(), "expected clean, got empty={:?} no_manifest={:?}",
            v.empty, v.no_manifest);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_strict_returns_clean_when_no_gitmodules() {
        // Plain repo without submodules — validation should be a no-op.
        let tmp = tempdir("val-no-gm");
        write_manifest(&tmp.join("module-a"));

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(v.is_clean());

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_strict_rejects_submodule_without_manifest() {
        // CLAUDE.md v2.2 doctrine: cada submódulo debe ser módulo o agrupador.
        // Un submódulo con archivos pero sin __manifest__.py ni .gitmodules
        // (e.g. repo de docs/scripts) → 422 submodule_no_manifest.
        let tmp = tempdir("val-no-manifest");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "docs"]
    path = docs
    url = https://github.com/acme/docs.git
[submodule "real-module"]
    path = real-module
    url = https://github.com/acme/real-module.git
"#).unwrap();
        // docs/ has files but is NOT a module (no __manifest__.py, no
        // nested .gitmodules) — basurero de scripts.
        fs::create_dir_all(tmp.join("docs")).unwrap();
        fs::write(tmp.join("docs/README.md"), "# docs\n").unwrap();
        fs::write(tmp.join("docs/setup.sh"), "#!/bin/bash\n").unwrap();
        // real-module/ is a valid module
        write_manifest(&tmp.join("real-module"));

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(!v.is_clean());
        assert!(v.empty.is_empty());
        assert!(v.no_manifest.iter().any(|p| p.contains("docs")),
            "expected 'docs' in no_manifest, got: {:?}", v.no_manifest);
        assert!(!v.no_manifest.iter().any(|p| p.contains("real-module")));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn validate_submodules_strict_accepts_grouper_with_nested_modules() {
        // Padre > Hijo > Nieto: un nivel intermedio sin manifest pero con
        // .gitmodules apuntando a módulos válidos NO se rechaza.
        let tmp = tempdir("val-grouper");
        fs::write(tmp.join(".gitmodules"), r#"
[submodule "group"]
    path = group
    url = https://github.com/acme/group.git
"#).unwrap();
        fs::create_dir_all(tmp.join("group")).unwrap();
        fs::write(tmp.join("group/.gitmodules"), r#"
[submodule "child-mod"]
    path = child-mod
    url = https://github.com/acme/child-mod.git
"#).unwrap();
        write_manifest(&tmp.join("group/child-mod"));

        let v = validate_submodules_strict(&tmp.to_string_lossy());
        assert!(v.is_clean(),
            "grouper-with-nested-module should be clean, got empty={:?} no_manifest={:?}",
            v.empty, v.no_manifest);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_addons_state_skips_dotfile_dirs_and_empty_modules() {
        let tmp = tempdir("addons-state");
        // Module with .nkr-source containing content_hash
        let m1 = tmp.join("mod_a");
        fs::create_dir_all(&m1).unwrap();
        fs::write(m1.join(".nkr-source"),
            "repo_url=https://x\nref=main\nsha=abc\n\
             content_hash=1234567890abcdef1234567890abcdef12345678\n").unwrap();
        // Module without .nkr-source
        let m2 = tmp.join("mod_b");
        fs::create_dir_all(&m2).unwrap();
        // Hidden dir (should be skipped)
        let hidden = tmp.join(".nkr-tmp-foo");
        fs::create_dir_all(&hidden).unwrap();
        // Plain file at top level (should be skipped)
        fs::write(tmp.join("README.md"), "x").unwrap();

        let state = read_addons_state(&tmp.to_string_lossy());
        assert_eq!(state.len(), 2);
        assert_eq!(state.get("mod_a"),
            Some(&"1234567890abcdef1234567890abcdef12345678".to_string()));
        assert_eq!(state.get("mod_b"), Some(&"".to_string()));
        assert!(!state.contains_key(".nkr-tmp-foo"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn atomic_swap_dirs_exchanges_contents() {
        // El path principal usa renameat2(RENAME_EXCHANGE). Test sobre
        // el FS de /tmp (típicamente tmpfs o ext4 sobre Linux 3.15+).
        // Si en el sandbox del test el FS no lo soporta (ENOSYS), el
        // fallback se activa automáticamente — el test sigue pasando.
        let root = tempdir("swap");
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("marker_a.txt"), "from-a").unwrap();
        fs::write(b.join("marker_b.txt"), "from-b").unwrap();

        atomic_swap_dirs(&a.to_string_lossy(), &b.to_string_lossy()).unwrap();

        // Post-swap: a contiene lo que era b, y viceversa.
        assert!(a.join("marker_b.txt").exists(),
            "tras swap, dir 'a' debería tener marker_b");
        assert!(b.join("marker_a.txt").exists(),
            "tras swap, dir 'b' debería tener marker_a");
        assert!(!a.join("marker_a.txt").exists());
        assert!(!b.join("marker_b.txt").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn renameat2_exchange_fails_when_one_missing() {
        // Si uno de los paths no existe, renameat2 debe fallar (no debe
        // crear el path missing). El caller maneja esto creando ambos dirs
        // antes de llamar atomic_swap_dirs.
        let root = tempdir("swap-missing");
        let a = root.join("a");
        let b = root.join("b");  // no existe
        fs::create_dir_all(&a).unwrap();

        let res = renameat2_exchange(&a.to_string_lossy(), &b.to_string_lossy());
        assert!(res.is_err(), "swap con path missing debe fallar");

        let _ = fs::remove_dir_all(&root);
    }
}
